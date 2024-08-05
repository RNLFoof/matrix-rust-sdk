use std::{future, ops::Deref, sync::Arc};

use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_common::store_locks::CrossProcessStoreLock;
use ruma::{OwnedUserId, UserId};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::warn;

use super::{DeviceChanges, IdentityChanges, LockableCryptoStore};
use crate::{
    olm::InboundGroupSession,
    store,
    store::{Changes, DynCryptoStore, IntoCryptoStore, RoomKeyInfo, RoomKeyWithheldInfo},
    CryptoStoreError,
    CryptoStoreError::CryptoStoreWrapperMigrationError,
    GossippedSecret, OwnUserIdentityData,
};

/// A wrapper for crypto store implementations that adds update notifiers.
///
/// This is shared between [`StoreInner`] and
/// [`crate::verification::VerificationStore`].
#[derive(Debug)]
pub(crate) struct CryptoStoreWrapper {
    user_id: OwnedUserId,
    store: Arc<DynCryptoStore>,

    /// The sender side of a broadcast stream that is notified whenever we get
    /// an update to an inbound group session.
    room_keys_received_sender: broadcast::Sender<Vec<RoomKeyInfo>>,

    /// The sender side of a broadcast stream that is notified whenever we
    /// receive an `m.room_key.withheld` message.
    room_keys_withheld_received_sender: broadcast::Sender<Vec<RoomKeyWithheldInfo>>,

    /// The sender side of a broadcast channel which sends out secrets we
    /// received as a `m.secret.send` event.
    secrets_broadcaster: broadcast::Sender<GossippedSecret>,

    /// The sender side of a broadcast channel which sends out devices and user
    /// identities which got updated or newly created.
    identities_broadcaster:
        broadcast::Sender<(Option<OwnUserIdentityData>, IdentityChanges, DeviceChanges)>,
}

const STORE_WRAPPER_VERSION_KEY: &str = "CryptoStoreWrapper_VERSION";
const STORE_WRAPPER_VERSION: u32 = 1;

async fn migrate(wrapper: &CryptoStoreWrapper, old_version: u32) -> Result<(), CryptoStoreError> {
    if old_version < 1 {
        // this
        wrapper
            .store
            .set_custom_value(STORE_WRAPPER_VERSION_KEY, 1_u32.to_le_bytes().into())
            .await?;
    }

    // if old_version < 2 { // Not `else if` because we want to run each migration
    // in turn     // Future migration
    //     ... Something like marking all users as dirty?
    //     // Update the current version to complete this migration
    //     wrapper.store.set_custom_value(STORE_WRAPPER_VERSION_KEY,
    // 2_u32.to_le_bytes().into()).await?; }

    Ok(())
}

async fn read_store_version(store: &DynCryptoStore) -> Result<Option<u32>, CryptoStoreError> {
    let value = store.get_custom_value(STORE_WRAPPER_VERSION_KEY).await?;
    Ok(match value {
        Some(u8_vec) => Some(u32::from_le_bytes(u8_vec.try_into().map_err(|_| {
            CryptoStoreWrapperMigrationError("Failed to read store version".to_owned())
        })?)),
        _ => None,
    })
}
impl CryptoStoreWrapper {
    pub(crate) async fn new(
        user_id: &UserId,
        store: impl IntoCryptoStore,
    ) -> Result<Self, CryptoStoreError> {
        let room_keys_received_sender = broadcast::Sender::new(10);
        let room_keys_withheld_received_sender = broadcast::Sender::new(10);
        let secrets_broadcaster = broadcast::Sender::new(10);
        // The identities broadcaster is responsible for user identities as well as
        // devices, that's why we increase the capacity here.
        let identities_broadcaster = broadcast::Sender::new(20);

        let store_wrapper = Self {
            user_id: user_id.to_owned(),
            store: store.into_crypto_store(),
            room_keys_received_sender,
            room_keys_withheld_received_sender,
            secrets_broadcaster,
            identities_broadcaster,
        };

        // Simple wrapper level migration handling
        let old_version = read_store_version(store_wrapper.store.as_ref()).await?.unwrap_or(0);
        let new_version = STORE_WRAPPER_VERSION;
        if new_version < old_version {
            // Backward migration
            return Err(CryptoStoreWrapperMigrationError(
                "The database format changed in an incompatible way".into(),
            ));
        }

        migrate(&store_wrapper, old_version).await.map_err(|e| {
            CryptoStoreWrapperMigrationError(format!("An Error occurred during migration {}", e))
        })?;

        let upgraded_version = read_store_version(store_wrapper.store.as_ref()).await?.unwrap_or(0);

        if upgraded_version != new_version {
            return Err(CryptoStoreWrapperMigrationError(
                "Migration did not upgrade up to the expected store version".into(),
            ));
        }

        Ok(store_wrapper)
    }

    /// Save the set of changes to the store.
    ///
    /// Also responsible for sending updates to the broadcast streams such as
    /// `room_keys_received_sender` and `secrets_broadcaster`.
    ///
    /// # Arguments
    ///
    /// * `changes` - The set of changes that should be stored.
    pub async fn save_changes(&self, changes: Changes) -> store::Result<()> {
        let room_key_updates: Vec<_> =
            changes.inbound_group_sessions.iter().map(RoomKeyInfo::from).collect();

        let withheld_session_updates: Vec<_> = changes
            .withheld_session_info
            .iter()
            .flat_map(|(room_id, session_map)| {
                session_map.iter().map(|(session_id, withheld_event)| RoomKeyWithheldInfo {
                    room_id: room_id.to_owned(),
                    session_id: session_id.to_owned(),
                    withheld_event: withheld_event.clone(),
                })
            })
            .collect();

        let secrets = changes.secrets.to_owned();
        let devices = changes.devices.to_owned();
        let identities = changes.identities.to_owned();

        self.store.save_changes(changes).await?;

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.room_keys_received_sender.send(room_key_updates);
        }

        if !withheld_session_updates.is_empty() {
            let _ = self.room_keys_withheld_received_sender.send(withheld_session_updates);
        }

        for secret in secrets {
            let _ = self.secrets_broadcaster.send(secret);
        }

        if !devices.is_empty() || !identities.is_empty() {
            // Mapping the devices and user identities from the read-only variant to one's
            // that contain side-effects requires our own identity. This is
            // guaranteed to be up-to-date since we just persisted it.
            let own_identity =
                self.store.get_user_identity(&self.user_id).await?.and_then(|i| i.into_own());

            let _ = self.identities_broadcaster.send((own_identity, identities, devices));
        }

        Ok(())
    }

    /// Save a list of inbound group sessions to the store.
    ///
    /// # Arguments
    ///
    /// * `sessions` - The sessions to be saved.
    /// * `backed_up_to_version` - If the keys should be marked as having been
    ///   backed up, the version of the backup.
    ///
    /// Note: some implementations ignore `backup_version` and assume the
    /// current backup version, which is normally the same.
    pub async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> store::Result<()> {
        let room_key_updates: Vec<_> = sessions.iter().map(RoomKeyInfo::from).collect();
        self.store.save_inbound_group_sessions(sessions, backed_up_to_version).await?;

        if !room_key_updates.is_empty() {
            // Ignore the result. It can only fail if there are no listeners.
            let _ = self.room_keys_received_sender.send(room_key_updates);
        }
        Ok(())
    }

    /// Receive notifications of room keys being received as a [`Stream`].
    ///
    /// Each time a room key is updated in any way, an update will be sent to
    /// the stream. Updates that happen at the same time are batched into a
    /// [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_received_stream(&self) -> impl Stream<Item = Vec<RoomKeyInfo>> {
        let stream = BroadcastStream::new(self.room_keys_received_sender.subscribe());
        Self::filter_errors_out_of_stream(stream, "room_keys_received_stream")
    }

    /// Receive notifications of received `m.room_key.withheld` messages.
    ///
    /// Each time an `m.room_key.withheld` is received and stored, an update
    /// will be sent to the stream. Updates that happen at the same time are
    /// batched into a [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, a warning will be
    /// logged and items will be dropped.
    pub fn room_keys_withheld_received_stream(
        &self,
    ) -> impl Stream<Item = Vec<RoomKeyWithheldInfo>> {
        let stream = BroadcastStream::new(self.room_keys_withheld_received_sender.subscribe());
        Self::filter_errors_out_of_stream(stream, "room_keys_withheld_received_stream")
    }

    /// Receive notifications of gossipped secrets being received and stored in
    /// the secret inbox as a [`Stream`].
    pub fn secrets_stream(&self) -> impl Stream<Item = GossippedSecret> {
        let stream = BroadcastStream::new(self.secrets_broadcaster.subscribe());
        Self::filter_errors_out_of_stream(stream, "secrets_stream")
    }

    /// Returns a stream of newly created or updated cryptographic identities.
    ///
    /// This is just a helper method which allows us to build higher level
    /// device and user identity streams.
    pub(super) fn identities_stream(
        &self,
    ) -> impl Stream<Item = (Option<OwnUserIdentityData>, IdentityChanges, DeviceChanges)> {
        let stream = BroadcastStream::new(self.identities_broadcaster.subscribe());
        Self::filter_errors_out_of_stream(stream, "identities_stream")
    }

    /// Helper for *_stream functions: filters errors out of the stream,
    /// creating a new Stream.
    ///
    /// `BroadcastStream`s gives us `Result`s which can fail with
    /// `BroadcastStreamRecvError` if the reader falls behind. That's annoying
    /// to work with, so here we just emit a warning and drop the errors.
    fn filter_errors_out_of_stream<ItemType>(
        stream: BroadcastStream<ItemType>,
        stream_name: &str,
    ) -> impl Stream<Item = ItemType>
    where
        ItemType: 'static + Clone + Send,
    {
        let stream_name = stream_name.to_owned();
        stream.filter_map(move |result| {
            future::ready(match result {
                Ok(r) => Some(r),
                Err(BroadcastStreamRecvError::Lagged(lag)) => {
                    warn!("{stream_name} missed {lag} updates");
                    None
                }
            })
        })
    }

    /// Creates a `CrossProcessStoreLock` for this store, that will contain the
    /// given key and value when hold.
    pub(crate) fn create_store_lock(
        &self,
        lock_key: String,
        lock_value: String,
    ) -> CrossProcessStoreLock<LockableCryptoStore> {
        CrossProcessStoreLock::new(LockableCryptoStore(self.store.clone()), lock_key, lock_value)
    }
}

impl Deref for CryptoStoreWrapper {
    type Target = DynCryptoStore;

    fn deref(&self) -> &Self::Target {
        self.store.deref()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::user_id;

    use crate::{
        store::{
            crypto_store_wrapper::{
                read_store_version, STORE_WRAPPER_VERSION, STORE_WRAPPER_VERSION_KEY,
            },
            CryptoStore, CryptoStoreWrapper, MemoryStore,
        },
        CryptoStoreError,
    };

    #[async_test]
    async fn test_migration() {
        let store = MemoryStore::new();

        let version = store.get_custom_value(STORE_WRAPPER_VERSION_KEY).await.unwrap();

        assert!(version.is_none());

        let alice_id = user_id!("@alice:localhost");
        let wrapper = CryptoStoreWrapper::new(alice_id, store).await.unwrap();

        let version = read_store_version(wrapper.store.as_ref()).await.unwrap().unwrap();

        assert_eq!(STORE_WRAPPER_VERSION, version);
    }

    #[async_test]
    async fn test_backward_migration_should_error() {
        let store = MemoryStore::new();

        store
            .set_custom_value(STORE_WRAPPER_VERSION_KEY, 4_u32.to_le_bytes().into())
            .await
            .unwrap();

        let alice_id = user_id!("@alice:localhost");
        let wrapper = CryptoStoreWrapper::new(alice_id, store).await;
        assert!(wrapper.is_err());
        let error = wrapper.unwrap_err();
        assert_matches!(error, CryptoStoreError::CryptoStoreWrapperMigrationError(_));
    }
}
