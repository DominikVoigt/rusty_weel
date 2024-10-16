use std::sync::Arc;

use crate::{data_types::{ChooseVariant, HTTPParams}, dsl_realization::Result};

pub trait DSL {
    /**
     * Implements the invokation of external functionalities
     */
    fn call(
        self: Arc<Self>,
        id: &str,
        endpoint_url: &str,
        parameters: HTTPParams,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<()>;

    /**
     * Implements script tasks that do not need to invoke functionalities
     */
    fn manipulate(self: Arc<Self>, id: &str, code: &str) -> Result<()>;

    fn loop_exec(
        &self,
        condition: Result<bool>,
        lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()>;

    fn pre_test(&self, condition: &str) -> Result<bool>;

    fn post_test(&self, condition: &str) -> Result<bool>;

    /**
     * Implements the parallel/event gateway -> Executes the provided branches in parallel
     * `wait` - None: Will wait for all branches to return
     *        - Value: Will wait for the specified number of branches to return
     * `cancel` - Determines on which task the termination of a branch is decided on: either the first or last task in a branch
     */
    fn parallel_do(
        &self,
        wait: Option<u32>,
        cancel: &str,
        lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()>;

    /**
     * One of the parallel branches within a parallel do, has to create the threads and then wait for sync with the parallel_do for execution
     */
    fn parallel_branch(
        &self,
        /*data: &str,*/ lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()>;

    /**
     * Guards critical block
     * All sections with the same mutex_id share the mutex
     */
    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()>;

    /**
     * Implements the BPMN choice in the dsl
     */
    fn choose(self: Arc<Self>, variant: ChooseVariant, lambda: impl Fn() -> Result<()> + Sync) -> Result<()>;

    /**
     * Implements a single branch of the BPMN choice
     * lambda will be executed if the provided condition is true, or if search mode is true (as the start position might be in any branch)
     */
    fn alternative(self: Arc<Self>, condition: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()>;

    /**
     * Implements otherwise branch of the BPMN choice
     * lambda will be executed none of the `alternative` branch conditions are true, or if search mode is true (as the start position might be in any branch)
     */
    fn otherwise(self: Arc<Self>, lambda: impl Fn() -> Result<()> + Sync);

    fn stop(self: Arc<Self>, id: &str) -> Result<()>;
    fn terminate(self: Arc<Self>) -> Result<()>;
}
