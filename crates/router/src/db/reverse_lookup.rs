use super::{MockDb, Store};
use crate::{
    errors::{self, CustomResult},
    types::storage::{
        enums,
        reverse_lookup::{ReverseLookup, ReverseLookupNew},
    },
};

#[async_trait::async_trait]
pub trait ReverseLookupInterface {
    async fn insert_reverse_lookup(
        &self,
        _new: ReverseLookupNew,
        _storage_scheme: enums::MerchantStorageScheme,
    ) -> CustomResult<ReverseLookup, errors::StorageError>;
    async fn get_lookup_by_lookup_id(
        &self,
        _id: &str,
        _storage_scheme: enums::MerchantStorageScheme,
    ) -> CustomResult<ReverseLookup, errors::StorageError>;
}

#[cfg(not(feature = "kv_store"))]
mod storage {
    use error_stack::IntoReport;

    use super::{ReverseLookupInterface, Store};
    use crate::{
        connection,
        errors::{self, CustomResult},
        types::storage::{
            enums,
            reverse_lookup::{ReverseLookup, ReverseLookupNew},
        },
    };

    #[async_trait::async_trait]
    impl ReverseLookupInterface for Store {
        async fn insert_reverse_lookup(
            &self,
            new: ReverseLookupNew,
            _storage_scheme: enums::MerchantStorageScheme,
        ) -> CustomResult<ReverseLookup, errors::StorageError> {
            let conn = connection::pg_connection_write(self).await?;
            new.insert(&conn).await.map_err(Into::into).into_report()
        }

        async fn get_lookup_by_lookup_id(
            &self,
            id: &str,
            _storage_scheme: enums::MerchantStorageScheme,
        ) -> CustomResult<ReverseLookup, errors::StorageError> {
            let conn = connection::pg_connection_read(self).await?;
            ReverseLookup::find_by_lookup_id(id, &conn)
                .await
                .map_err(Into::into)
                .into_report()
        }
    }
}

#[cfg(feature = "kv_store")]
mod storage {
    use error_stack::{IntoReport, ResultExt};
    use redis_interface::SetnxReply;
    use storage_impl::redis::kv_store::RedisConnInterface;

    use super::{ReverseLookupInterface, Store};
    use crate::{
        connection,
        errors::{self, CustomResult},
        types::storage::{
            enums, kv,
            reverse_lookup::{ReverseLookup, ReverseLookupNew},
        },
        utils::{db_utils, storage_partitioning::PartitionKey},
    };

    #[async_trait::async_trait]
    impl ReverseLookupInterface for Store {
        async fn insert_reverse_lookup(
            &self,
            new: ReverseLookupNew,
            storage_scheme: enums::MerchantStorageScheme,
        ) -> CustomResult<ReverseLookup, errors::StorageError> {
            match storage_scheme {
                data_models::MerchantStorageScheme::PostgresOnly => {
                    let conn = connection::pg_connection_write(self).await?;
                    new.insert(&conn).await.map_err(Into::into).into_report()
                }
                data_models::MerchantStorageScheme::RedisKv => {
                    let created_rev_lookup = ReverseLookup {
                        lookup_id: new.lookup_id.clone(),
                        sk_id: new.sk_id.clone(),
                        pk_id: new.pk_id.clone(),
                        source: new.source.clone(),
                    };
                    let combination = &created_rev_lookup.pk_id;
                    match self
                        .get_redis_conn()
                        .map_err(Into::<errors::StorageError>::into)?
                        .serialize_and_set_key_if_not_exist(
                            &created_rev_lookup.lookup_id,
                            &created_rev_lookup,
                        )
                        .await
                    {
                        Ok(SetnxReply::KeySet) => {
                            let redis_entry = kv::TypedSql {
                                op: kv::DBOperation::Insert {
                                    insertable: kv::Insertable::ReverseLookUp(new),
                                },
                            };
                            self.push_to_drainer_stream::<ReverseLookup>(
                                redis_entry,
                                PartitionKey::MerchantIdPaymentIdCombination { combination },
                            )
                            .await
                            .change_context(errors::StorageError::KVError)?;

                            Ok(created_rev_lookup)
                        }
                        Ok(SetnxReply::KeyNotSet) => Err(errors::StorageError::DuplicateValue {
                            entity: "reverse_lookup",
                            key: Some(created_rev_lookup.lookup_id.clone()),
                        })
                        .into_report(),
                        Err(er) => Err(er).change_context(errors::StorageError::KVError),
                    }
                }
            }
        }

        async fn get_lookup_by_lookup_id(
            &self,
            id: &str,
            storage_scheme: enums::MerchantStorageScheme,
        ) -> CustomResult<ReverseLookup, errors::StorageError> {
            let database_call = || async {
                let conn = connection::pg_connection_read(self).await?;
                ReverseLookup::find_by_lookup_id(id, &conn)
                    .await
                    .map_err(Into::into)
                    .into_report()
            };

            match storage_scheme {
                data_models::MerchantStorageScheme::PostgresOnly => database_call().await,
                data_models::MerchantStorageScheme::RedisKv => {
                    let redis_conn = self
                        .get_redis_conn()
                        .map_err(Into::<errors::StorageError>::into)?;

                    let redis_fut =
                        redis_conn.get_and_deserialize_key::<ReverseLookup>(id, "ReverseLookup");

                    db_utils::try_redis_get_else_try_database_get(redis_fut, database_call).await
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ReverseLookupInterface for MockDb {
    async fn insert_reverse_lookup(
        &self,
        new: ReverseLookupNew,
        _storage_scheme: enums::MerchantStorageScheme,
    ) -> CustomResult<ReverseLookup, errors::StorageError> {
        let reverse_lookup_insert = ReverseLookup::from(new);
        self.reverse_lookups
            .lock()
            .await
            .push(reverse_lookup_insert.clone());
        Ok(reverse_lookup_insert)
    }

    async fn get_lookup_by_lookup_id(
        &self,
        lookup_id: &str,
        _storage_scheme: enums::MerchantStorageScheme,
    ) -> CustomResult<ReverseLookup, errors::StorageError> {
        self.reverse_lookups
            .lock()
            .await
            .iter()
            .find(|reverse_lookup| reverse_lookup.lookup_id == lookup_id)
            .ok_or(
                errors::StorageError::ValueNotFound(format!(
                    "No reverse lookup found for lookup_id = {}",
                    lookup_id
                ))
                .into(),
            )
            .cloned()
    }
}
