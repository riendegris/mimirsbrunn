use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::domain::model::query_parameters::QueryParameters;
use crate::domain::ports::query::Query;
use crate::domain::usecases::{Error as UseCaseError, UseCase};

// FIXME Maybe need two use cases.... one for
pub struct SearchDocuments<D> {
    pub query: Box<dyn Query<Doc = D> + Send + Sync + 'static>,
}

impl<D> SearchDocuments<D> {
    pub fn new(query: Box<dyn Query<Doc = D> + Send + Sync + 'static>) -> Self {
        SearchDocuments { query }
    }
}

pub struct SearchDocumentsParameters {
    pub query_parameters: QueryParameters,
}

#[async_trait]
impl<D: DeserializeOwned + Send + Sync + 'static> UseCase for SearchDocuments<D> {
    type Res = Vec<D>;
    type Param = SearchDocumentsParameters;

    async fn execute(&self, param: Self::Param) -> Result<Self::Res, UseCaseError> {
        self.query
            .search_documents(param.query_parameters)
            .await
            .map_err(|err| UseCaseError::Execution {
                source: Box::new(err),
            })
    }
}
