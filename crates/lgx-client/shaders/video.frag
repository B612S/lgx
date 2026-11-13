#version 450
layout(location = 0) in vec2 v_uv;
layout(binding = 0) uniform sampler2D tex_y;
layout(binding = 1) uniform sampler2D tex_uv;
layout(location = 0) out vec4 out_color;

// HEVC full-range BT.709, matching the server's conversion.
void main() {
    float y = texture(tex_y, v_uv).r;
    vec2 c = texture(tex_uv, v_uv).rg - vec2(0.5, 0.5);
    vec3 rgb = vec3(
        y + 1.5748 * c.y,
        y - 0.1873 * c.x - 0.4681 * c.y,
        y + 1.8556 * c.x
    );
    out_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
