use std::error::Error;

use crate::data_types::HTTPRequest;

pub trait DSL<T: Error> {
    /**
     * Implements the invokation of external functionalities
     */
    fn call(
        &self, 
        label: &str,
        endpoint_url: &str,
        parameters: HTTPRequest,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<(), T>;

    /**
     * Implements script tasks that do not need to invoke functionalities
     */
    fn manipulate(&self, label: &str, name: Option<&str>, code: &str) -> Result<(), T>;

    fn loop_exec(&self, condition: bool, lambda: impl Fn() + Sync) -> Result<(), T>;

    fn pre_test(&self, condition: &str) -> bool;

    fn post_test(&self, condition: &str) -> bool;

    /**
     * Implements the parallel/event gateway -> Executes the provided branches in parallel
     * `wait` - None: Will wait for all branches to return
     *        - Value: Will wait for the specified number of branches to return
     * `cancel` - Determines on which task the termination of a branch is decided on: either the first or last task in a branch
     */
    fn parallel_do(&self, wait: Option<u32>, cancel: &str, lambda: impl Fn() + Sync) -> Result<(), T>;

    /**
     * One of the parallel branches within a parallel do, has to create the threads and then wait for sync with the parallel_do for execution
     */
    fn parallel_branch(&self, data: &str, lambda: impl Fn() + Sync) -> Result<(), T>;

    /**
     * Guards critical block
     * All sections with the same mutex_id share the mutex
     */
    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() + Sync) -> Result<(), T>;

    /**
     * Implements
     */
    fn choose(&self, variant: &str, lambda: impl Fn() + Sync) -> Result<(), T>;

    fn alternative(&self, condition: &str, lambda: impl Fn() + Sync) -> Result<(), T>;

    fn stop(&self, label: &str)  -> Result<(), T>;
}
