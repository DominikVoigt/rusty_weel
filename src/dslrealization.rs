use crate::model::Parameters;
use crate::dsl::DSL;

    pub struct Weel {}

    impl DSL for Weel
    {
        fn call(label: &str, endpoint_url: &str, parameters: Parameters, 
            // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
            prepare_code: Option<&str>, 
            update_code: Option<&str>, 
            finalize_code: Option<&str>, 
            rescue_code: Option<&str>) {
            println!("Calling with parameters: {:?}", parameters)
        }
    
        fn parallel_do(wait: Option<u32>, cancel: &str, start_branches: fn() -> ()) {
            println!("Calling parallel_do");
            println!("Executing lambda");
            start_branches();
        }
    
        fn parallel_branch(data: &str, lambda: fn(String) -> ()) {
            println!("Executing parallel branch");
            lambda(data.to_string())
        }
        
        fn choose(variant: &str, lambda: fn() -> ()) {
            println!("Executing choose");
            lambda();
        }
        
        fn alternative(condition: &str, lambda: fn() -> ()) {
            println!("Executing alternative, ignoring condition: {}", condition)
        }
        
        fn manipulate(label: &str, name: &str, code: &str) {
            println!("Calling manipulate")
        }
        
        fn loop_exec(condition: bool, lambda: fn() -> ()) {
            println!("Executing loop!");
        }
        
        fn pre_test(condition: &str) -> bool {
            false
        }
        
        fn post_test(condition: &str) -> bool {
            true
        }
        
        fn stop(label: &str) {
            println!("Stopping... just kidding")
        }
        
        fn critical_do(mutex_id: &str, lambda: fn(String) -> ()) {
            println!("in critical do")
        }
    }