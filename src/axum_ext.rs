use axum::routing::MethodRouter;
use axum::Router;

/// from https://stackoverflow.com/questions/75355826/route-paths-with-or-without-of-trailing-slashes-in-rust-axum
pub trait RouterExt<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn directory_route(self, path: &str, method_router: MethodRouter<S>) -> Self;
}

impl<S> RouterExt<S> for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn directory_route(self, path: &str, method_router: MethodRouter<S>) -> Self {
        self.route(path, method_router.clone())
            .route(&format!("{path}/"), method_router)
    }
}
