//! `lgx-caps`: print what this machine can do for lgx.
//!
//! The output is the same information the server and client use to pick an
//! encoder/decoder, so a bug report can include one command. `--json` is for
//! scripts and tests.

use lgx_caps::drm::{drm_nodes, DrmNode};
use lgx_caps::select::{decode_plan, encoder_for_node, va_caps_for_node};
use lgx_caps::vaapi::{probe as probe_vaapi, VaCaps};
use lgx_caps::vulkan::{devices as vulkan_devices, VkDeviceInfo};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: lgx-caps [--json]");
        return;
    }
    let json = args.iter().any(|a| a == "--json");
    let nodes = drm_nodes();
    let va: Vec<VaCaps> = nodes.iter().filter(|n| n.render).filter_map(probe_vaapi).collect();
    let vulkan = vulkan_devices().unwrap_or_default();
    let cuda = lgx_caps::cuda::device_names().unwrap_or_default();

    if json {
        print_json(&nodes, &va, &vulkan, &cuda);
    } else {
        print_human(&nodes, &va, &vulkan, &cuda);
    }
}

fn print_human(nodes: &[DrmNode], va: &[VaCaps], vulkan: &[VkDeviceInfo], cuda: &[String]) {
    println!("DRM nodes:");
    if nodes.is_empty() {
        println!("  (none)");
    }
    for n in nodes {
        println!(
            "  {:<11} {:<10} vendor=0x{:04x} device=0x{:04x} {:>3}:{} {}",
            n.name,
            if n.driver.is_empty() { "unknown" } else { &n.driver },
            n.vendor,
            n.device,
            n.major,
            n.minor,
            if n.render { "render" } else { "card" },
        );
    }
    println!("VAAPI:");
    if va.is_empty() {
        println!("  (no usable VAAPI device)");
    }
    for c in va {
        println!(
            "  {:<22} decode={} (10-bit {}) encode={} (low_power {}) vpp={}",
            c.node.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            yesno(c.hevc_decode),
            yesno(c.hevc_decode_10),
            yesno(c.hevc_encode),
            yesno(c.hevc_encode_low_power),
            yesno(c.vpp),
        );
        println!("    vendor: {}", if c.vendor.is_empty() { "unknown" } else { &c.vendor });
    }
    println!("Vulkan:");
    if vulkan.is_empty() {
        println!("  (no Vulkan device)");
    }
    for d in vulkan {
        let id = format!("{:04x}:{:04x}", d.vendor_id, d.device_id);
        println!(
            "  [{}] {} ({} {}, driver {})",
            d.index,
            d.name,
            d.device_type,
            id,
            if d.driver.is_empty() { "unknown" } else { &d.driver },
        );
        println!(
            "    drm render={} video_decode_h265={} video_encode_h265={} dmabuf import={} export={}",
            d.render.map(|(a, b)| format!("{a}:{b}")).unwrap_or_else(|| "unreported".into()),
            yesno(d.video_decode_h265),
            yesno(d.video_encode_h265),
            yesno(d.dmabuf_import),
            yesno(d.dmabuf_export),
        );
    }
    println!("CUDA:");
    if cuda.is_empty() {
        println!("  (none)");
    }
    for name in cuda {
        println!("  {name}");
    }

    let cuda_ok = !cuda.is_empty();
    println!("Plan:");
    for n in nodes.iter().filter(|n| n.render) {
        let caps = va_caps_for_node(va, n);
        let enc = encoder_for_node(Some(n), caps, cuda_ok);
        println!(
            "  server on {}: encoder={}",
            n.name,
            enc.map(|e| e.name()).unwrap_or("none"),
        );
    }
    for d in vulkan {
        let plan = decode_plan(d, nodes, va, cuda_ok);
        let names: Vec<&str> = plan.iter().map(|b| b.name()).collect();
        println!("  client on [{}] {}: {}", d.index, d.name, names.join(" -> "));
    }
}

fn print_json(nodes: &[DrmNode], va: &[VaCaps], vulkan: &[VkDeviceInfo], cuda: &[String]) {
    let cuda_ok = !cuda.is_empty();
    let mut s = String::from("{");
    s.push_str("\"drm\":[");
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"name\":\"{}\",\"path\":\"{}\",\"render\":{},\"major\":{},\"minor\":{},\"driver\":\"{}\",\"vendor\":{},\"device\":{}}}",
            esc(&n.name),
            esc(&n.path.to_string_lossy()),
            n.render,
            n.major,
            n.minor,
            esc(&n.driver),
            n.vendor,
            n.device,
        ));
    }
    s.push_str("],\"vaapi\":[");
    for (i, c) in va.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"node\":\"{}\",\"vendor\":\"{}\",\"hevc_decode\":{},\"hevc_decode_10\":{},\"hevc_encode\":{},\"hevc_encode_low_power\":{},\"vpp\":{}}}",
            esc(&c.node.to_string_lossy()),
            esc(&c.vendor),
            c.hevc_decode,
            c.hevc_decode_10,
            c.hevc_encode,
            c.hevc_encode_low_power,
            c.vpp,
        ));
    }
    s.push_str("],\"vulkan\":[");
    for (i, d) in vulkan.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let render = match d.render {
            Some((a, b)) => format!("[{a},{b}]"),
            None => "null".into(),
        };
        s.push_str(&format!(
            "{{\"index\":{},\"name\":\"{}\",\"driver\":\"{}\",\"type\":\"{}\",\"vendor_id\":{},\"device_id\":{},\"render\":{},\"video_decode_h265\":{},\"video_encode_h265\":{},\"dmabuf_import\":{},\"dmabuf_export\":{}}}",
            d.index,
            esc(&d.name),
            esc(&d.driver),
            esc(&d.device_type),
            d.vendor_id,
            d.device_id,
            render,
            d.video_decode_h265,
            d.video_encode_h265,
            d.dmabuf_import,
            d.dmabuf_export,
        ));
    }
    s.push_str("],\"cuda\":[");
    for (i, name) in cuda.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("\"{}\"", esc(name)));
    }
    s.push_str("],\"plan\":{\"server\":[");
    let mut first = true;
    for n in nodes.iter().filter(|n| n.render) {
        if !first {
            s.push(',');
        }
        first = false;
        let enc = encoder_for_node(Some(n), va_caps_for_node(va, n), cuda_ok);
        s.push_str(&format!(
            "{{\"node\":\"{}\",\"encoder\":\"{}\"}}",
            esc(&n.name),
            enc.map(|e| e.name()).unwrap_or("none"),
        ));
    }
    s.push_str("],\"client\":[");
    for (i, d) in vulkan.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let plan: Vec<String> = decode_plan(d, nodes, va, cuda_ok)
            .iter()
            .map(|b| format!("\"{}\"", b.name()))
            .collect();
        s.push_str(&format!(
            "{{\"device\":{},\"path\":[{}]}}",
            d.index,
            plan.join(","),
        ));
    }
    s.push_str("]}}");
    println!("{s}");
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

fn yesno(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
