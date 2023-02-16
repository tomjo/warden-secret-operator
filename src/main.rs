#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;

use std::{env};

fn main() {
    env::set_var("RUST_LOG", "info");
    env_logger::init();
    let args: Vec<String> = env::args().collect();


}
