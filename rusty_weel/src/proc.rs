use core::str;
use std::{collections::HashMap, num::ParseFloatError, process::{Command, Stdio}};

type Result<T> = std::result::Result<T, Error>;

/**
 * Returns the proportional set size of the proces
 */
pub fn get_prop_set_size() -> Result<String> {
    let pid = std::process::id();
    get_prop_set_size_pid(pid)
}

pub fn get_cpu_times() -> Result<HashMap<String, String>> {
    let pid = std::process::id();
    get_cpu_times_pid(pid)
}

/**
 * Returns the proportional set size of the proces
 */
fn get_prop_set_size_pid(pid: u32) -> Result<String> {
    let path = format!("/proc/{pid}/smaps");
    // Measures memory in MB
    let awk_programm = "/.*Pss.*/ {Total+=$2} END {print Total/1024}";

    let cat = Command::new("cat").arg(path).stdout(Stdio::piped()).spawn().unwrap();
    let awk = Command::new("awk").arg(awk_programm).stdin(Stdio::from(cat.stdout.unwrap())).stdout(Stdio::piped()).spawn().unwrap();
    let output = awk.wait_with_output()?;
    let output_utf8 = str::from_utf8(&output.stdout)?.trim();
    Ok(output_utf8.to_owned())
}

/**
 * Returns the proportional set size of the proces
 */
fn get_cpu_times_pid(pid: u32) -> Result<HashMap<String, String>> {
    let path = format!("/proc/{pid}/stat");
    // gets utime, stime, cutime, cstime
    let awk_programm = r#"{print "{\"utime\": \"" $14"\", \"stime\": \"" $15"\", \"cutime\": \""  $16"\", \"cstime\": \"" $17"\"}"}"#;

    let cat = Command::new("cat").arg(path).stdout(Stdio::piped()).spawn().unwrap();
    let awk = Command::new("awk").arg(awk_programm).stdin(Stdio::from(cat.stdout.unwrap())).stdout(Stdio::piped()).spawn().unwrap();
    let output = awk.wait_with_output()?;
    let output_utf8 = str::from_utf8(&output.stdout)?.trim();
    Ok(serde_json::from_str(output_utf8).expect("String structure is static, cannot fail"))
}

#[derive(derive_more::From, Debug)]
pub enum Error {
    ParseFloatError(ParseFloatError),
    IOError(std::io::Error),
    Utf8Error(str::Utf8Error)
}

#[cfg(test)]
mod test{

    use super::*;
    
    #[test]
    fn test_mem() {
        let mem = get_prop_set_size().unwrap();
        println!("{mem}");
    }

    #[test]
    fn test_time() {
        let times = get_cpu_times_pid(4048).unwrap();
        assert_eq!(times.len(), 4);
        println!("{:?}", times);
    }
}