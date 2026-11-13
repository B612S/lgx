#version 450
layout(push_constant) uniform Push {
    vec4 rect;
    vec4 uv;
    float alpha;
} pc;
layout(location = 0) out vec2 v_uv;
void main() {
    vec2 t = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 p = mix(pc.rect.xy, pc.rect.zw, t);
    v_uv = mix(pc.uv.xy, pc.uv.zw, t);
    gl_Position = vec4(p, 0.0, 1.0);
}
