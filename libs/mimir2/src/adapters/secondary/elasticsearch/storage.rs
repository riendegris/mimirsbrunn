use async_trait::async_trait;
use futures::future::TryFutureExt;
use futures::stream::Stream;
use std::convert::TryFrom;

use super::internal;
use super::ElasticsearchStorage;
use crate::domain::model::{
    configuration::{self, Configuration},
    document::Document,
    index::{Index, IndexVisibility},
    stats::InsertStats,
};
use crate::domain::ports::storage::{Error as StorageError, Storage};

#[async_trait]
impl Storage for ElasticsearchStorage {
    // This function delegates to elasticsearch the creation of the index. But since this
    // function returns nothing, we follow with a find index to return some details to the caller.
    async fn create_container(&self, config: Configuration) -> Result<Index, StorageError> {
        let config = internal::IndexConfiguration::try_from(config).map_err(|err| {
            StorageError::ContainerCreationError {
                source: Box::new(err),
            }
        })?;
        let name = config.name.clone();
        self.create_index(config)
            .and_then(|_| {
                self.find_index(name.clone()).and_then(|res| {
                    futures::future::ready(
                        res.ok_or(internal::Error::ElasticsearchUnknownIndex { index: name }),
                    )
                })
            })
            .await
            .map_err(|err| StorageError::ContainerCreationError {
                source: Box::new(err),
            })
    }

    // FIXME Move details to impl ElasticsearchStorage.
    async fn delete_container(&self, index: String) -> Result<(), StorageError> {
        self.delete_index(index.clone())
            .await
            .map_err(|err| StorageError::ContainerDeletionError {
                source: Box::new(err),
            })
    }

    // FIXME Move details to impl ElasticsearchStorage.
    async fn find_container(&self, index: String) -> Result<Option<Index>, StorageError> {
        self.find_index(index)
            .await
            .map_err(|err| StorageError::ContainerSearchError {
                source: Box::new(err),
            })
    }

    // FIXME Move details to impl ElasticsearchStorage.
    async fn insert_documents<S>(
        &self,
        index: String,
        documents: S,
    ) -> Result<InsertStats, StorageError>
    where
        S: Stream<Item = Box<dyn Document + Send + Sync + 'static>> + Send + Sync + Unpin + 'static,
    {
        self.add_pipeline(
            String::from(include_str!(
                "../../../../../../config/pipeline/indexed_at.json"
            )),
            String::from("indexed_at"),
        )
        .await
        .map_err(|err| StorageError::DocumentInsertionError {
            source: Box::new(err),
        })?;
        self.insert_documents_in_index(index, documents)
            .await
            .map(InsertStats::from)
            .map_err(|err| StorageError::DocumentInsertionError {
                source: Box::new(err),
            })
    }

    // Maybe all this should be run in some kind of transaction.
    async fn publish_index(
        &self,
        index: Index,
        visibility: IndexVisibility,
    ) -> Result<(), StorageError> {
        self.refresh_index(index.name.clone())
            .await
            .map_err(|err| StorageError::IndexPublicationError {
                source: Box::new(err),
            })?;

        let previous_indices = self.get_previous_indices(&index).await.map_err(|err| {
            StorageError::IndexPublicationError {
                source: Box::new(err),
            }
        })?;

        let base_index = configuration::root_doctype_dataset(&index.doc_type, &index.dataset);

        if !previous_indices.is_empty() {
            self.remove_alias(previous_indices.clone(), base_index.clone())
                .await
                .map_err(|err| StorageError::IndexPublicationError {
                    source: Box::new(err),
                })?;
        }
        self.add_alias(vec![index.name.clone()], base_index.clone())
            .await
            .map_err(|err| StorageError::IndexPublicationError {
                source: Box::new(err),
            })?;

        if visibility == IndexVisibility::Public {
            let base_index = configuration::root_doctype(&index.doc_type);
            if !previous_indices.is_empty() {
                self.remove_alias(previous_indices.clone(), base_index.clone())
                    .await
                    .map_err(|err| StorageError::IndexPublicationError {
                        source: Box::new(err),
                    })?;
            }
            self.add_alias(vec![index.name.clone()], base_index.clone())
                .await
                .map_err(|err| StorageError::IndexPublicationError {
                    source: Box::new(err),
                })?;
        }

        Ok(())
    }
}
