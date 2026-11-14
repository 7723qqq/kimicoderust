pub mod backends;
#[path = "configSection.rs"]
pub mod config_section;
pub mod interface;

#[path = "atomicDocumentStore.rs"]
pub mod atomic_document_store;
#[path = "fileStorageService.rs"]
pub mod file_storage_service;

pub use atomic_document_store::{
    AtomicDocumentGuard, JsonAtomicDocumentStore, TomlAtomicDocumentStore,
};
pub use file_storage_service::{
    FileStorageService, StorageAppendOptions, StorageListOptions, StorageReadRange, StorageResult,
    StorageWriteOptions, to_storage_io_error,
};
