use crate::model::Parameters;

pub trait DSL {
    /**
     * Implements the invokation of external functionalities
     */
    fn call(
        &self, 
        label: &str,
        endpoint_url: &str,
        parameters: Parameters,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    );

    /**
     * Implements script tasks that do not need to invoke functionalities
     */
    fn manipulate(&self, label: &str, name: Option<&str>, code: &str);

    fn loop_exec(&self, condition: bool, lambda: impl Fn());

    fn pre_test(&self, condition: &str) -> bool;

    fn post_test(&self, condition: &str) -> bool;

    /**
     * Implements the parallel/event gateway -> Executes the provided branches in parallel
     * `wait` - None: Will wait for all branches to return
     *        - Value: Will wait for the specified number of branches to return
     * `cancel` - Determines on which task the termination of a branch is decided on: either the first or last task in a branch
     */
    fn parallel_do(&self, wait: Option<u32>, cancel: &str, lambda: impl Fn());

    /**
     * One of the parallel branches within a parallel do, has to create the threads and then wait for sync with the parallel_do for execution
     */
    fn parallel_branch(&self, data: &str, lambda: impl Fn(&str));

    /**
     * Guards critical block
     * All sections with the same mutex_id share the mutex
     */
    fn critical_do(&self, mutex_id: &str, lambda: impl Fn());

    /**
     * Implements
     */
    fn choose(&self, variant: &str, lambda: impl Fn());

    fn alternative(&self, condition: &str, lambda: impl Fn());

    fn stop(&self, label: &str);
}
