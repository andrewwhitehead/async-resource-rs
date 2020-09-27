use futures_lite::future::Boxed as BoxFuture;

use super::Executor;

pub struct GlobalExecutor;

impl Executor for GlobalExecutor {
    fn spawn_obj(&self, task: BoxFuture<()>) {
        async_global_executor::spawn(task).detach()
    }
}
