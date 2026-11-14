use sha2::{Digest, Sha256};

use crate::human::store::backend::backend::BlobBackend;
use crate::human::store::types::StoreError;

pub async fn sha256_hex(data: &str) -> String {
    let digest = Sha256::digest(data.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub async fn write_blob<B>(blobs: &B, data: &str) -> Result<String, StoreError>
where
    B: BlobBackend + ?Sized,
{
    let reference = sha256_hex(data).await;
    if !blobs.has(&reference).await? {
        blobs.write(&reference, data).await?;
    }
    Ok(reference)
}

pub async fn read_blob<B>(blobs: &B, reference: &str) -> Result<String, StoreError>
where
    B: BlobBackend + ?Sized,
{
    if !blobs.has(reference).await? {
        return Err(StoreError::new(
            "blob-missing",
            format!("blob {reference} is missing"),
        ));
    }
    let data = blobs.read(reference).await?;
    if sha256_hex(&data).await != reference {
        return Err(StoreError::new(
            "blob-crc",
            format!("blob {reference} failed its hash check"),
        ));
    }
    Ok(data)
}
