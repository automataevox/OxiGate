//! Pre-allocated, lock-free buffer pool to bound memory under traffic bursts.
use bytes::BytesMut;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;

pub const DEFAULT_BUF_CAPACITY: usize = 16 * 1024;
pub const DEFAULT_POOL_SIZE: usize = 4096;

#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<ArrayQueue<BytesMut>>,
    capacity: usize,
}

impl BufferPool {
    pub fn new(size: usize, buf_capacity: usize) -> Self {
        let size = size.max(64);
        let buf_capacity = buf_capacity.max(1024);
        let q = ArrayQueue::new(size);
        for _ in 0..size {
            let mut b = BytesMut::with_capacity(buf_capacity);
            b.reserve(buf_capacity);
            let _ = q.push(b);
        }
        Self { inner: Arc::new(q), capacity: buf_capacity }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_POOL_SIZE, DEFAULT_BUF_CAPACITY)
    }

    pub fn acquire(&self) -> PooledBuf {
        let buf = self.inner.pop().unwrap_or_else(|| {
            let mut b = BytesMut::with_capacity(self.capacity);
            b.reserve(self.capacity);
            b
        });
        PooledBuf { buf: Some(buf), pool: self.inner.clone(), capacity: self.capacity }
    }

    pub fn len(&self) -> usize { self.inner.len() }
    pub fn capacity(&self) -> usize { self.inner.capacity() }
}

pub struct PooledBuf {
    buf: Option<BytesMut>,
    pool: Arc<ArrayQueue<BytesMut>>,
    capacity: usize,
}

impl PooledBuf {
    pub fn as_mut(&mut self) -> &mut BytesMut {
        self.buf.as_mut().expect("PooledBuf already taken")
    }
    pub fn into_inner(mut self) -> BytesMut {
        self.buf.take().expect("PooledBuf already taken")
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let Some(mut b) = self.buf.take() {
            if b.capacity() <= self.capacity * 2 {
                b.clear();
                let _ = self.pool.push(b);
            }
        }
    }
}
