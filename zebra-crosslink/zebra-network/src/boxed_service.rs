//! A cloneable, `Send + Sync`, type-erased [`Service`].
//!
//! This is tower 0.5's `BoxCloneSyncService`, which tower 0.4 lacks. Its `BoxCloneService`
//! is not `Sync`, and the peer set and RPC server hold their services across threads.

use std::{
    fmt,
    task::{Context, Poll},
};

use futures::future::BoxFuture;
use tower::{Service, ServiceExt};

/// Any `Service<T, Response = U, Error = E> + Clone + Send + Sync`, behind a trait object.
///
/// Taking this type instead of a type parameter compiles the code that uses the service once,
/// in the crate that defines it, rather than in each crate and test binary that supplies one.
pub struct BoxCloneSyncService<T, U, E>(
    Box<
        dyn CloneService<T, Response = U, Error = E, Future = BoxFuture<'static, Result<U, E>>>
            + Send
            + Sync,
    >,
);

impl<T, U, E> BoxCloneSyncService<T, U, E> {
    /// Boxes `inner`.
    pub fn new<S>(inner: S) -> Self
    where
        S: Service<T, Response = U, Error = E> + Clone + Send + Sync + 'static,
        S::Future: Send + 'static,
    {
        let inner = inner.map_future(|f| Box::pin(f) as _);
        BoxCloneSyncService(Box::new(inner))
    }
}

impl<T, U, E> Service<T> for BoxCloneSyncService<T, U, E> {
    type Response = U;
    type Error = E;
    type Future = BoxFuture<'static, Result<U, E>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), E>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, request: T) -> Self::Future {
        self.0.call(request)
    }
}

impl<T, U, E> Clone for BoxCloneSyncService<T, U, E> {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl<T, U, E> fmt::Debug for BoxCloneSyncService<T, U, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxCloneSyncService").finish()
    }
}

trait CloneService<R>: Service<R> {
    fn clone_box(
        &self,
    ) -> Box<
        dyn CloneService<R, Response = Self::Response, Error = Self::Error, Future = Self::Future>
            + Send
            + Sync,
    >;
}

impl<R, T> CloneService<R> for T
where
    T: Service<R> + Send + Sync + Clone + 'static,
{
    fn clone_box(
        &self,
    ) -> Box<
        dyn CloneService<R, Response = T::Response, Error = T::Error, Future = T::Future>
            + Send
            + Sync,
    > {
        Box::new(self.clone())
    }
}
