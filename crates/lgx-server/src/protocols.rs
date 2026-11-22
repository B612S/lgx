//! Client bindings for Wayland protocols, generated from vendored XML.
//!
//! `wayland-protocols` no longer ships virtual-keyboard (removed upstream),
//! so all four protocols are generated here and their versions are pinned.

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

pub mod screencopy {
    protocol!("protocol/wlr-screencopy-unstable-v1.xml");
}

pub mod virtual_pointer {
    protocol!("protocol/wlr-virtual-pointer-unstable-v1.xml");
}

pub mod virtual_keyboard {
    protocol!("protocol/virtual-keyboard-unstable-v1.xml");
}

pub mod xdg_shell {
    protocol!("protocol/xdg-shell.xml");
}

pub mod linux_dmabuf {
    protocol!("protocol/linux-dmabuf-unstable-v1.xml");
}

pub mod xdg_output {
    protocol!("protocol/xdg-output-unstable-v1.xml");
}

// KDE's private screencast protocol. Used only by the `kde_probe` example to
// verify KDE capture without the portal consent dialog; KWin does not offer
// it as a stable API for regular clients.
pub mod kde_screencast {
    protocol!("protocol/zkde-screencast-unstable-v1.xml");
}
