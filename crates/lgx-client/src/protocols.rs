//! Wayland protocol bindings generated from vendored XML.

macro_rules! protocol {
    ($path:expr) => {
        #[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
        #[allow(non_upper_case_globals, non_snake_case, unused_imports, missing_docs)]
        pub mod client {
            use wayland_client;
            use wayland_client::protocol::*;

            pub mod __interfaces {
                use wayland_client::protocol::__interfaces::*;
                wayland_scanner::generate_interfaces!($path);
            }
            use self::__interfaces::*;

            wayland_scanner::generate_client_code!($path);
        }
    };
}

pub mod shortcuts_inhibit {
    protocol!("protocol/keyboard-shortcuts-inhibit-unstable-v1.xml");
}
