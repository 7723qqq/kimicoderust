use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ComparisonOp {
    #[serde(rename = "$eq", default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Value>,
    #[serde(rename = "$ne", default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<Value>,
    #[serde(rename = "$gt", default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<Value>,
    #[serde(rename = "$gte", default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<Value>,
    #[serde(rename = "$lt", default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<Value>,
    #[serde(rename = "$lte", default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<Value>,
    #[serde(rename = "$in", default, skip_serializing_if = "Option::is_none")]
    pub in_: Option<Vec<Value>>,
    #[serde(rename = "$nin", default, skip_serializing_if = "Option::is_none")]
    pub nin: Option<Vec<Value>>,
    #[serde(rename = "$exists", default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
}

pub type QueryFilter = Map<String, Value>;

#[async_trait]
pub trait IQuery<T: Send + Sync + 'static>: Send + Sync {
    fn where_(&mut self, filter: QueryFilter);
    fn where_column(&mut self, column: String, bounds: ColumnBounds);
    fn order_by(&mut self, field: String, dir: Option<SortDir>);
    fn limit(&mut self, n: usize);
    fn cursor(&mut self, cursor: Option<String>);
    async fn execute(&self) -> Result<Page<T>, WireAsyncError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IndexDef {
    Value {
        name: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unique: Option<bool>,
    },
    Compound {
        name: String,
        #[serde(rename = "groupBy")]
        group_by: String,
        #[serde(rename = "orderBy")]
        order_by: String,
    },
    Text {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WriteOp {
    Put {
        collection: String,
        key: String,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<HashMap<String, f64>>,
    },
    Delete {
        collection: String,
        key: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_signals: Option<HashMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_primaries: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub struct QueryStoreRebuiltError;

impl fmt::Display for QueryStoreRebuiltError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the query-store was rebuilt while the operation was in flight")
    }
}

impl Error for QueryStoreRebuiltError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnPageQuery {
    pub column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<SortDir>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<QueryFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<ColumnBounds>,
    pub limit: usize,
}

#[async_trait]
pub trait IQueryStore: Send + Sync {
    async fn put_value(
        &self,
        collection: &str,
        key: &str,
        value: Value,
        columns: Option<HashMap<String, f64>>,
    ) -> Result<(), WireAsyncError>;
    async fn batch(&self, ops: Vec<WriteOp>) -> Result<(), WireAsyncError>;
    async fn delete(&self, collection: &str, key: &str) -> Result<(), WireAsyncError>;
    async fn get_value(&self, collection: &str, key: &str) -> Result<Option<Value>, WireAsyncError>;
    async fn get_many_values(
        &self,
        collection: &str,
        keys: &[String],
    ) -> Result<HashMap<String, Value>, WireAsyncError>;
    fn query_values(&self, collection: &str) -> Box<dyn IQuery<Value>>;
    async fn page_by_column_values(
        &self,
        collection: &str,
        query: ColumnPageQuery,
    ) -> Result<Page<Value>, WireAsyncError>;
    async fn ensure_index(&self, collection: &str, def: IndexDef) -> Result<(), WireAsyncError>;
    async fn list_keys(&self, collection: &str) -> Result<Vec<String>, WireAsyncError>;
    async fn drop_collection(&self, collection: &str) -> Result<(), WireAsyncError>;
    async fn get_checkpoint(&self, source: &str) -> Result<Option<Checkpoint>, WireAsyncError>;
    async fn set_checkpoint(
        &self,
        source: &str,
        checkpoint: Checkpoint,
        expected_store_epoch: Option<u64>,
    ) -> Result<(), WireAsyncError>;
    fn store_epoch(&self) -> u64;
    async fn close(&self) -> Result<(), WireAsyncError>;
}

#[async_trait]
pub trait IQueryStoreExt: IQueryStore {
    async fn put<T>(
        &self,
        collection: &str,
        key: &str,
        value: T,
        columns: Option<HashMap<String, f64>>,
    ) -> Result<(), WireAsyncError>
    where
        T: Serialize + Send + Sync + 'static,
    {
        self.put_value(collection, key, serde_json::to_value(value)?, columns)
            .await
    }

    async fn get<T>(&self, collection: &str, key: &str) -> Result<Option<T>, WireAsyncError>
    where
        T: for<'de> Deserialize<'de> + Send + Sync + 'static,
    {
        match self.get_value(collection, key).await? {
            Some(value) => Ok(Some(serde_json::from_value(value)?)),
            None => Ok(None),
        }
    }

    async fn get_many<T>(
        &self,
        collection: &str,
        keys: &[String],
    ) -> Result<HashMap<String, T>, WireAsyncError>
    where
        T: for<'de> Deserialize<'de> + Send + Sync + 'static,
    {
        let values = self.get_many_values(collection, keys).await?;
        let mut out = HashMap::with_capacity(values.len());
        for (key, value) in values {
            out.insert(key, serde_json::from_value(value)?);
        }
        Ok(out)
    }

    fn query<T>(&self, collection: &str) -> Box<dyn IQuery<T>>
    where
        T: for<'de> Deserialize<'de> + Send + Sync + 'static,
    {
        Box::new(TypedQuery {
            inner: self.query_values(collection),
            marker: PhantomData,
        })
    }

    async fn page_by_column<T>(
        &self,
        collection: &str,
        query: ColumnPageQuery,
    ) -> Result<Page<T>, WireAsyncError>
    where
        T: for<'de> Deserialize<'de> + Send + Sync + 'static,
    {
        let page = self.page_by_column_values(collection, query).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for value in page.items {
            items.push(serde_json::from_value(value)?);
        }
        Ok(Page {
            items,
            next_cursor: page.next_cursor,
        })
    }
}

impl<T> IQueryStoreExt for T where T: IQueryStore + ?Sized {}

struct TypedQuery<T> {
    inner: Box<dyn IQuery<Value>>,
    marker: PhantomData<fn() -> T>,
}

#[async_trait]
impl<T> IQuery<T> for TypedQuery<T>
where
    T: for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    fn where_(&mut self, filter: QueryFilter) {
        self.inner.where_(filter);
    }

    fn where_column(&mut self, column: String, bounds: ColumnBounds) {
        self.inner.where_column(column, bounds);
    }

    fn order_by(&mut self, field: String, dir: Option<SortDir>) {
        self.inner.order_by(field, dir);
    }

    fn limit(&mut self, n: usize) {
        self.inner.limit(n);
    }

    fn cursor(&mut self, cursor: Option<String>) {
        self.inner.cursor(cursor);
    }

    async fn execute(&self) -> Result<Page<T>, WireAsyncError> {
        let page = self.inner.execute().await?;
        let mut items = Vec::with_capacity(page.items.len());
        for value in page.items {
            items.push(serde_json::from_value(value)?);
        }
        Ok(Page {
            items,
            next_cursor: page.next_cursor,
        })
    }
}
