#version 450
layout(binding = 0) uniform sampler2D osd;
layout(push_constant) uniform Push {
    vec4 rect;
    vec4 uv;
    float alpha;
} pc;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;
void main() {
    vec4 c = texture(osd, v_uv);
    out_color = vec4(c.rgb, c.a * pc.alpha);
}
