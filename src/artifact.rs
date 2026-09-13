use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::body::{Body, Bytes, HttpBody};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use uuid::Uuid;

pub const ARTIFACT_DIRECTORY: &str = "artifacts";
pub const MAX_ARTIFACT_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Debug)]
pub enum UploadError {
    Body(axum::Error),
    Io(io::Error),
    TooLarge,
}

#[derive(Debug)]
pub struct ArtifactFiles {
    directory: PathBuf,
}

pub struct ArtifactBody {
    file: tokio::fs::File,
    remaining: u64,
}

impl HttpBody for ArtifactBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let capacity = usize::try_from(self.remaining.min(64 * 1024)).unwrap();
        let mut buffer = vec![0; capacity];
        let mut read = ReadBuf::new(&mut buffer);
        match Pin::new(&mut self.file).poll_read(context, &mut read) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.remaining = 0;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Ok(())) if read.filled().is_empty() => {
                self.remaining = 0;
                Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "artifact file is shorter than its recorded size",
                ))))
            }
            Poll::Ready(Ok(())) => {
                let length = read.filled().len();
                self.remaining -= u64::try_from(length).unwrap();
                buffer.truncate(length);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(buffer)))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

impl ArtifactFiles {
    pub fn open(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn write(&self, id: Uuid, contents: &[u8]) -> io::Result<()> {
        let temporary = self.directory.join(format!("{id}.tmp"));
        let destination = self.path(id);
        let result = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, destination)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub fn read(&self, id: Uuid) -> io::Result<Vec<u8>> {
        fs::read(self.path(id))
    }

    pub async fn write_body(&self, id: Uuid, mut body: Body) -> Result<u64, UploadError> {
        self.write_body_with_limit(id, &mut body, MAX_ARTIFACT_BYTES)
            .await
    }

    async fn write_body_with_limit(
        &self,
        id: Uuid,
        body: &mut Body,
        limit: usize,
    ) -> Result<u64, UploadError> {
        let temporary = self.temporary_path(id);
        let destination = self.path(id);
        let result = async {
            let mut file = tokio::fs::File::create(&temporary)
                .await
                .map_err(UploadError::Io)?;
            let mut size = 0_usize;
            loop {
                let frame = std::future::poll_fn(|context| {
                    std::pin::Pin::new(&mut *body).poll_frame(context)
                })
                .await;
                let Some(frame) = frame else {
                    break;
                };
                let frame = frame.map_err(UploadError::Body)?;
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                size = size.checked_add(data.len()).ok_or(UploadError::TooLarge)?;
                if size > limit {
                    return Err(UploadError::TooLarge);
                }
                file.write_all(&data).await.map_err(UploadError::Io)?;
            }
            file.sync_all().await.map_err(UploadError::Io)?;
            drop(file);
            tokio::fs::rename(&temporary, &destination)
                .await
                .map_err(UploadError::Io)?;
            tokio::fs::File::open(&self.directory)
                .await
                .map_err(UploadError::Io)?
                .sync_all()
                .await
                .map_err(UploadError::Io)?;
            Ok(u64::try_from(size).expect("artifact size fits in u64"))
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(temporary).await;
            let _ = tokio::fs::remove_file(destination).await;
        }
        result
    }

    pub async fn body(&self, id: Uuid, expected_size: u64) -> io::Result<ArtifactBody> {
        let file = tokio::fs::File::open(self.path(id)).await?;
        if file.metadata().await?.len() != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact file size does not match its database record",
            ));
        }
        Ok(ArtifactBody {
            file,
            remaining: expected_size,
        })
    }

    pub fn remove(&self, id: Uuid) -> io::Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub async fn remove_async(&self, id: Uuid) -> io::Result<()> {
        match tokio::fs::remove_file(self.path(id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn remove_untracked(
        &self,
        tracked: &HashSet<Uuid>,
        minimum_age: Duration,
    ) -> io::Result<usize> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let id = Uuid::parse_str(name).ok();
            if id.is_some_and(|id| tracked.contains(&id)) {
                continue;
            }
            if id.is_some() || name.ends_with(".tmp") {
                let age = entry.metadata()?.modified()?.elapsed().unwrap_or_default();
                if age < minimum_age {
                    continue;
                }
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.directory.join(id.to_string())
    }

    fn temporary_path(&self, id: Uuid) -> PathBuf {
        self.directory.join(format!("{id}.tmp"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_reads_removes_and_reconciles_files() {
        let directory = tempdir().unwrap();
        let files = ArtifactFiles::open(directory.path()).unwrap();
        let kept = Uuid::new_v4();
        let orphan = Uuid::new_v4();
        files.write(kept, b"kept").unwrap();
        files.write(orphan, b"orphan").unwrap();
        fs::write(directory.path().join("interrupted.tmp"), b"partial").unwrap();
        fs::write(directory.path().join("operator-note"), b"leave me").unwrap();

        assert_eq!(files.read(kept).unwrap(), b"kept");
        assert_eq!(
            files
                .remove_untracked(&HashSet::from([kept]), Duration::ZERO)
                .unwrap(),
            2
        );
        assert_eq!(files.read(kept).unwrap(), b"kept");
        assert!(!directory.path().join(orphan.to_string()).exists());
        assert!(directory.path().join("operator-note").exists());
        files.remove(kept).unwrap();
        files.remove(kept).unwrap();
    }

    #[tokio::test]
    async fn streamed_upload_enforces_the_actual_byte_limit() {
        let directory = tempdir().unwrap();
        let files = ArtifactFiles::open(directory.path()).unwrap();
        let id = Uuid::new_v4();
        let mut body = Body::from("four");
        assert!(matches!(
            files.write_body_with_limit(id, &mut body, 3).await,
            Err(UploadError::TooLarge)
        ));
        assert!(!files.path(id).exists());
        assert!(!files.temporary_path(id).exists());
    }
}
