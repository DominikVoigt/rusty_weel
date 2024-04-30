use crate::model::Parameters;

pub trait DSL {
    /**
     * Implements the invokation of external functionalities
     */    
    fn call(label: &str, endpoint_url: &str, parameters: Parameters, 
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>, 
        update_code: Option<&str>, 
        finalize_code: Option<&str>, 
        rescue_code: Option<&str>
    );

    /**
     * Implements script tasks that do not need to invoke functionalities
     */
    fn manipulate(label: &str, name: &str, code: &str);

    fn loop_exec(condition: bool, lambda: fn() -> ());

    fn pre_test(condition: &str) -> bool;

    fn post_test(condition: &str) -> bool;

    /**
     * Implements the parallel/event gateway -> Executes the provided branches in parallel
     *
     * `wait` - None: Will wait for all branches to return
     *        - Value: Will wait for the specified number of branches to return 
     * `cancel` - Determines on which task the termination of a branch is decided on: either the first or last task in a branch
     */
    fn parallel_do(wait: Option<u32>, cancel: &str, lambda: fn() -> ());

    /**
     * One of the parallel branches within a parallel do, has to create the threads and then wait for sync with the parallel_do for execution
     */
    fn parallel_branch(data: &str, lambda: fn(String) -> ());

    /**
     * Guards critical block
     * All sections with the same mutex_id share the mutex
     */
    fn critical_do(mutex_id: &str, lambda: fn(String) -> ());

    /**
     * Implements 
     */
    fn choose(variant: &str, lambda: fn() -> ());

    fn alternative(condition: &str, lambda: fn() -> ());

    fn stop(label: &str);


}

pub mod model {


}