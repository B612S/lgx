//! GPU colour conversion: RGB (tiled dma-buf) -> NV12 in CPU memory.
//!
//! The tiled buffer cannot be read by the CPU, so it is imported into an EGL
//! context, converted with a fragment shader into two render targets (R8 for Y,
//! RG8 for UV) and read back as a packed NV12 frame that ffmpeg/VAAPI accepts.

// Unsafe FFI wrappers: every `unsafe fn` here forwards to the driver and its
// contract is described at the call site, so the per-function `# Safety`
// section and the argument-count heuristic are skipped deliberately.
#![allow(clippy::missing_safety_doc, clippy::too_many_arguments)]


use std::ffi::{c_char, c_void, CString};

const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_NONE: i32 = 0x3038;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES3_BIT: i32 = 0x0040;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;

type Display = *mut c_void;
type Config = *mut c_void;
type Context = *mut c_void;
type Surface = *mut c_void;
type Image = *mut c_void;

#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
    fn eglInitialize(dpy: Display, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(dpy: Display, attribs: *const i32, configs: *mut Config, n: i32, num: *mut i32) -> u32;
    fn eglCreateContext(dpy: Display, cfg: Config, share: Context, attribs: *const i32) -> Context;
    fn eglCreatePbufferSurface(dpy: Display, cfg: Config, attribs: *const i32) -> Surface;
    fn eglMakeCurrent(dpy: Display, draw: Surface, read: Surface, ctx: Context) -> u32;
    fn eglGetError() -> i32;
}

#[link(name = "GLESv2")]
extern "C" {
    fn glGenTextures(n: i32, tex: *mut u32);
    fn glBindTexture(target: u32, tex: u32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
    fn glGenFramebuffers(n: i32, fb: *mut u32);
    fn glBindFramebuffer(target: u32, fb: u32);
    fn glFramebufferTexture2D(target: u32, attachment: u32, tex_target: u32, tex: u32, level: i32);
    fn glDrawBuffers(n: i32, bufs: *const u32);
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glViewport(x: i32, y: i32, w: i32, h: i32);
    fn glCreateShader(typ: u32) -> u32;
    fn glShaderSource(shader: u32, count: i32, string: *const *const c_char, len: *const i32);
    fn glCompileShader(shader: u32);
    fn glGetShaderiv(shader: u32, pname: u32, param: *mut i32);
    fn glCreateProgram() -> u32;
    fn glAttachShader(prog: u32, shader: u32);
    fn glLinkProgram(prog: u32);
    fn glGetProgramiv(prog: u32, pname: u32, param: *mut i32);
    fn glUseProgram(prog: u32);
    fn glGenVertexArrays(n: i32, va: *mut u32);
    fn glBindVertexArray(va: u32);
    fn glDrawArrays(mode: u32, first: i32, count: i32);
    fn glReadBuffer(mode: u32);
    fn glReadPixels(x: i32, y: i32, w: i32, h: i32, format: u32, typ: u32, data: *mut c_void);
    fn glGetError() -> u32;
    fn glFinish();
    fn glFlush();
    fn glGenBuffers(n: i32, b: *mut u32);
    fn glBindBuffer(target: u32, b: u32);
    fn glBufferData(target: u32, size: isize, data: *const c_void, usage: u32);
    fn glMapBufferRange(target: u32, offset: isize, len: isize, access: u32) -> *mut c_void;
    fn glUnmapBuffer(target: u32) -> u8;
    fn glFenceSync(condition: u32, flags: u32) -> *mut c_void;
    fn glClientWaitSync(sync: *mut c_void, flags: u32, timeout: u64) -> u32;
    fn glDeleteSync(sync: *mut c_void);
    fn glUniform1i(loc: i32, v: i32) -> ();
    fn glGetUniformLocation(prog: u32, name: *const c_char) -> i32;
}

type CreateImage = unsafe extern "C" fn(Display, Context, u32, *mut c_void, *const i32) -> Image;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

/// Double-buffered asynchronous readback: the GPU writes frame N into one PBO
/// while the CPU consumes frame N-1 from the other.
pub struct PboRead {
    pbos: [u32; 2],
    fences: [*mut c_void; 2],
    fbo: u32,
    cur: usize,
    pub size: usize,
    pub valid: [bool; 2],
}

impl GlConv {
    pub unsafe fn pbo_new(&self, size: usize) -> Option<PboRead> {
        let mut pbos = [0u32; 2];
        glGenBuffers(2, pbos.as_mut_ptr());
        for pbo in pbos {
            glBindBuffer(0x88EB, pbo);
            glBufferData(0x88EB, size as isize, std::ptr::null(), 0x88E8); // STREAM_READ
        }
        let mut fbo = 0u32;
        glGenFramebuffers(1, &mut fbo);
        let err = glGetError();
        if pbos[0] == 0 || fbo == 0 || err != 0 {
            eprintln!("lgx-server: pbo_new failed pbos={:?} fbo={} err={:#x}", pbos, fbo, err);
            return None;
        }
        Some(PboRead { pbos, fences: [std::ptr::null_mut(); 2], fbo, cur: 0, size, valid: [false; 2] })
    }

    /// Starts an async readback of `tex` into the current PBO.
    pub unsafe fn pbo_start(&self, pr: &mut PboRead, tex: u32, w: u32, h: u32) {
        glBindFramebuffer(0x8D40, pr.fbo);
        glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, tex, 0);
        glDrawBuffers(1, [0x8CE0u32].as_ptr());
        glViewport(0, 0, w as i32, h as i32);
        glBindBuffer(0x88EB, pr.pbos[pr.cur]);
        glReadPixels(0, 0, w as i32, h as i32, 0x80E1, 0x1401, std::ptr::null_mut()); // BGRA
        if !pr.fences[pr.cur].is_null() {
            glDeleteSync(pr.fences[pr.cur]);
        }
        pr.fences[pr.cur] = glFenceSync(0x9117, 0);
        pr.valid[pr.cur] = true;
        pr.cur = 1 - pr.cur;
    }

    /// Waits for the frame started in the previous call and copies it out.
    pub unsafe fn pbo_finish(&self, pr: &mut PboRead, out: &mut [u8]) -> bool {
        let idx = 1 - pr.cur;
        if !pr.valid[idx] || pr.fences[idx].is_null() {
            return false;
        }
        glClientWaitSync(pr.fences[idx], 0x00000001, 1_000_000_000);
        glDeleteSync(pr.fences[idx]);
        pr.fences[idx] = std::ptr::null_mut();
        glBindBuffer(0x88EB, pr.pbos[idx]);
        let ptr = glMapBufferRange(0x88EB, 0, pr.size as isize, 0x0001); // READ
        if ptr.is_null() {
            pr.valid[idx] = false;
            return false;
        }
        std::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), pr.size.min(out.len()));
        glUnmapBuffer(0x88EB);
        pr.valid[idx] = false;
        true
    }
}

pub struct Planes {
    pub y_tex: u32,
    pub uv_tex: u32,
    fbo_y: u32,
    fbo_uv: u32,
}

pub struct GlConv {
    _dpy: Display,
    _ctx: Context,
    _surf: Surface,
    pub tex_rgb: u32,
    prog: u32,
    vao: u32,
    mode_loc: i32,
    create_image: CreateImage,
    target_tex: ImageTargetTexture,
    rgb_cache: std::cell::RefCell<std::collections::HashMap<i32, u32>>,
}

impl GlConv {
    /// # Safety
    /// `gbm_dev` must be a live GBM device for the lifetime of the context.
    pub unsafe fn new(gbm_dev: *mut c_void) -> Option<Self> {
        unsafe {
            let get_platform: unsafe extern "C" fn(u32, *mut c_void, *const i32) -> Display =
                std::mem::transmute(eglGetProcAddress(CString::new("eglGetPlatformDisplayEXT").ok()?.as_ptr()));
            let dpy = get_platform(EGL_PLATFORM_GBM_KHR, gbm_dev, std::ptr::null());
            if dpy.is_null() {
                return None;
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(dpy, &mut major, &mut minor) == 0 {
                return None;
            }
            eglBindAPI(EGL_OPENGL_ES_API);
            let cfg_attribs = [EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE];
            let mut cfg: Config = std::ptr::null_mut();
            let mut n = 0;
            eglChooseConfig(dpy, cfg_attribs.as_ptr(), &mut cfg, 1, &mut n);
            let ctx_attribs = [0x3098, 3, EGL_NONE];
            let ctx = eglCreateContext(dpy, cfg, std::ptr::null_mut(), ctx_attribs.as_ptr());
            let pb = [EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE];
            let surf = eglCreatePbufferSurface(dpy, cfg, pb.as_ptr());
            if eglMakeCurrent(dpy, surf, surf, ctx) == 0 {
                return None;
            }
            let create_image: CreateImage = std::mem::transmute(eglGetProcAddress(CString::new("eglCreateImageKHR").ok()?.as_ptr()));
            let target_tex: ImageTargetTexture = std::mem::transmute(eglGetProcAddress(CString::new("glEGLImageTargetTexture2DOES").ok()?.as_ptr()));

            let mut tex_rgb = 0;
            glGenTextures(1, &mut tex_rgb);
            glBindTexture(0x0DE1, tex_rgb);
            glTexParameteri(0x0DE1, 0x2801, 0x2601);
            glTexParameteri(0x0DE1, 0x2800, 0x2601);

            let vs = r#"#version 300 es
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}"#;
            let fs = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D tex;
uniform int mode;
layout(location=0) out vec4 outColor;
// Full-range BT.709: this is what SDL's NV12 renderer assumes, so the client
// sees exactly the RGB that was captured.
void main() {
    vec3 c = texture(tex, v_uv).rgb;
    float y = dot(c, vec3(0.2126, 0.7152, 0.0722));
    if (mode == 0) {
        outColor = vec4(y, 0.0, 0.0, 1.0);
    } else {
        // One bilinear sample at the centre of the 2x2 RGB block is already
        // the 2x2 average; four explicit taps only added texture work.
        vec3 c2 = texture(tex, v_uv).rgb;
        float y2 = dot(c2, vec3(0.2126, 0.7152, 0.0722));
        float cb = (c2.b - y2) / 1.8556;
        float cr = (c2.r - y2) / 1.5748;
        outColor = vec4(cb + 0.5, cr + 0.5, 0.0, 1.0);
    }
}"#;
            let prog = link(vs, fs)?;
            let mut vao = 0;
            glGenVertexArrays(1, &mut vao);
            glBindVertexArray(vao);
            glUseProgram(prog);
            let loc = glGetUniformLocation(prog, c"tex".as_ptr());
            glUniform1i(loc, 0);
            let mode_loc = glGetUniformLocation(prog, c"mode".as_ptr());
            glUniform1i(mode_loc, 0);

            Some(Self {
                _dpy: dpy,
                _ctx: ctx,
                _surf: surf,
                tex_rgb,
                prog,
                vao,
                mode_loc,
                create_image,
                target_tex,
                rgb_cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            })
        }
    }

    /// Imports the XRGB dma-buf once and returns its texture.
    pub unsafe fn import_rgb(&self, fd: i32, stride: u32, modifier: u64, w: u32, h: u32) -> Option<u32> {
        if let Some(tex) = self.rgb_cache.borrow().get(&fd) {
            return Some(*tex);
        }
        let attribs = [
            EGL_WIDTH, w as i32,
            EGL_HEIGHT, h as i32,
            0x3271, 0x3432_5258,
            0x3272, fd,
            0x3273, 0,
            0x3274, stride as i32,
            0x3443, (modifier & 0xffff_ffff) as i32,
            0x3444, (modifier >> 32) as i32,
            EGL_NONE,
        ];
        let img = (self.create_image)(self._dpy, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
        if img.is_null() {
            eprintln!("lgx-server: gl: rgb import failed err={:#x}", eglGetError());
            return None;
        }
        let mut tex = 0u32;
        glGenTextures(1, &mut tex);
        glBindTexture(0x0DE1, tex);
        glTexParameteri(0x0DE1, 0x2801, 0x2601);
        glTexParameteri(0x0DE1, 0x2800, 0x2601);
        (self.target_tex)(0x0DE1, img);
        if glGetError() != 0 {
            return None;
        }
        self.rgb_cache.borrow_mut().insert(fd, tex);
        Some(tex)
    }

    /// Imports a single-plane dma-buf as a texture (R8 or GR88).
    pub unsafe fn import_plane(
        &self,
        fd: i32,
        offset: u32,
        pitch: u32,
        modifier: u64,
        fourcc: u32,
        w: u32,
        h: u32,
    ) -> Option<u32> {
        let attribs = [
            EGL_WIDTH, w as i32,
            EGL_HEIGHT, h as i32,
            0x3271, fourcc as i32,
            0x3272, fd,
            0x3273, offset as i32,
            0x3274, pitch as i32,
            0x3443, (modifier & 0xffff_ffff) as i32,
            0x3444, (modifier >> 32) as i32,
            EGL_NONE,
        ];
        let img = (self.create_image)(self._dpy, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
        if img.is_null() {
            eprintln!("lgx-server: gl: plane import failed err={:#x}", eglGetError());
            return None;
        }
        let mut tex = 0u32;
        glGenTextures(1, &mut tex);
        glBindTexture(0x0DE1, tex);
        glTexParameteri(0x0DE1, 0x2801, 0x2600);
        glTexParameteri(0x0DE1, 0x2800, 0x2600);
        (self.target_tex)(0x0DE1, img);
        if glGetError() != 0 {
            return None;
        }
        Some(tex)
    }

    /// Renders the RGB texture into two single-plane dma-bufs (Y and UV).
    pub unsafe fn render_planes(
        &self,
        tex_rgb: u32,
        y_fd: i32,
        y_pitch: u32,
        uv_fd: i32,
        uv_pitch: u32,
        w: u32,
        h: u32,
    ) -> bool {
        let y = self.import_plane(y_fd, 0, y_pitch, 0, 0x2020_3852, w, h);
        let uv = self.import_plane(uv_fd, 0, uv_pitch, 0, 0x3838_5247, w / 2, h / 2);
        if std::env::var("LGX_GLDEBUG").is_ok() {
            eprintln!("lgx-server: plane textures y={:?} uv={:?}", y, uv);
        }
        match (y, uv) {
            (Some(y), Some(uv)) => {
                self.draw_into_planes(tex_rgb, y, uv, w, h);
                true
            }
            _ => false,
        }
    }

    /// Renders the RGB texture into raw Y/UV planes of a linear buffer
    /// (offsets + pitches given), used as a zero-copy VAAPI input.
    pub unsafe fn render_nv12_planes(
        &self,
        tex_rgb: u32,
        fd: i32,
        y_offset: u32,
        uv_offset: u32,
        pitch: u32,
        w: u32,
        h: u32,
    ) -> bool {
        let y = self.import_plane(fd, y_offset, pitch, 0, 0x2020_3852, w, h);
        let uv = self.import_plane(fd, uv_offset, pitch, 0, 0x3838_5247, w / 2, h / 2);
        match (y, uv) {
            (Some(y), Some(uv)) => {
                self.draw_plane(tex_rgb, y, w, h, 0);
                self.draw_plane(tex_rgb, uv, w / 2, h / 2, 1);
                self.finish();
                true
            }
            _ => false,
        }
    }

    /// Imports the two NV12 plane textures once and builds their framebuffers.
    pub unsafe fn prepare_planes(
        &self,
        fd: i32,
        uv_offset: u32,
        pitch: u32,
        w: u32,
        h: u32,
    ) -> Option<Planes> {
        let y_tex = self.import_plane(fd, 0, pitch, 0, 0x2020_3852, w, h)?;
        let uv_tex = self.import_plane(fd, uv_offset, pitch, 0, 0x3838_5247, w / 2, h / 2)?;
        let mut fbo_y = 0u32;
        let mut fbo_uv = 0u32;
        glGenFramebuffers(1, &mut fbo_y);
        glGenFramebuffers(1, &mut fbo_uv);
        glBindFramebuffer(0x8D40, fbo_y);
        glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, y_tex, 0);
        glDrawBuffers(1, [0x8CE0u32].as_ptr());
        let ok_y = glCheckFramebufferStatus(0x8D40) == 0x8CD5;
        glBindFramebuffer(0x8D40, fbo_uv);
        glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, uv_tex, 0);
        glDrawBuffers(1, [0x8CE0u32].as_ptr());
        let ok_uv = glCheckFramebufferStatus(0x8D40) == 0x8CD5;
        if !ok_y || !ok_uv {
            eprintln!("lgx-server: gl: plane framebuffer incomplete");
            return None;
        }
        Some(Planes { y_tex, uv_tex, fbo_y, fbo_uv })
    }

    /// Renders both planes into the cached framebuffers.
    pub unsafe fn draw_planes(&self, p: &Planes, tex_rgb: u32, w: u32, h: u32) {
        self.draw_into_fbo(p.fbo_y, tex_rgb, w, h, 0);
        self.draw_into_fbo(p.fbo_uv, tex_rgb, w / 2, h / 2, 1);
        if std::env::var_os("LGX_NO_GLFINISH").is_some() {
            glFlush();
        } else {
            glFinish();
        }
    }

    unsafe fn draw_into_fbo(&self, fbo: u32, tex_rgb: u32, w: u32, h: u32, mode: i32) {
        glBindFramebuffer(0x8D40, fbo);
        glViewport(0, 0, w as i32, h as i32);
        glUseProgram(self.prog);
        glUniform1i(self.mode_loc, mode);
        glBindVertexArray(self.vao);
        glBindTexture(0x0DE1, tex_rgb);
        glDrawArrays(0x0004, 0, 3);
    }

    /// Draws one plane of the NV12 target (mode 0 = Y, 1 = UV).
    pub unsafe fn draw_plane(&self, tex_rgb: u32, target: u32, w: u32, h: u32, mode: i32) {
        let mut fbo = 0u32;
        glGenFramebuffers(1, &mut fbo);
        glBindFramebuffer(0x8D40, fbo);
        glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, target, 0);
        let bufs = [0x8CE0u32];
        glDrawBuffers(1, bufs.as_ptr());
        glViewport(0, 0, w as i32, h as i32);
        glUseProgram(self.prog);
        let mode_loc = glGetUniformLocation(self.prog, c"mode".as_ptr());
        glUniform1i(mode_loc, mode);
        glBindVertexArray(self.vao);
        glBindTexture(0x0DE1, tex_rgb);
        glDrawArrays(0x0004, 0, 3);
    }

    /// Blocks until all previously issued GL work has finished.
    pub unsafe fn finish(&self) {
        glFinish();
    }

    /// Same as `draw_into` but creates a throwaway framebuffer for the pair.
    pub unsafe fn draw_into_planes(&self, tex_rgb: u32, tex_y: u32, tex_uv: u32, w: u32, h: u32) {
        self.draw_into(tex_rgb, tex_y, tex_uv, w, h)
    }

    /// Renders the RGB texture into the given Y/UV textures and kicks the GPU.
    pub unsafe fn draw_into(&self, tex_rgb: u32, tex_y: u32, tex_uv: u32, w: u32, h: u32) {
        let mut fbo = 0u32;
        glGenFramebuffers(1, &mut fbo);
        glBindFramebuffer(0x8D40, fbo);
        glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, tex_y, 0);
        glFramebufferTexture2D(0x8D40, 0x8CE1, 0x0DE1, tex_uv, 0);
        let bufs = [0x8CE0u32, 0x8CE1u32];
        glDrawBuffers(2, bufs.as_ptr());
        let st = glCheckFramebufferStatus(0x8D40);
        if st != 0x8CD5 {
            eprintln!("lgx-server: gl: target framebuffer incomplete {:#x}", st);
        }
        glViewport(0, 0, w as i32, h as i32);
        glUseProgram(self.prog);
        glBindVertexArray(self.vao);
        glBindTexture(0x0DE1, tex_rgb);
        glDrawArrays(0x0004, 0, 3);
        glFinish();
        if std::env::var("LGX_GLDEBUG").is_ok() {
            glReadBuffer(0x8CE0);
            let mut px = [0u8; 16];
            glReadPixels(0, 0, 2, 2, 0x1903, 0x1401, px.as_mut_ptr() as *mut c_void);
            eprintln!("lgx-server: gl debug Y pixels {:?}", px);
            glReadBuffer(0x8CE1);
            let mut uv = [0u8; 16];
            glReadPixels(0, 0, 1, 1, 0x8227, 0x1401, uv.as_mut_ptr() as *mut c_void);
            eprintln!("lgx-server: gl debug UV pixels {:?}", uv);
        }
    }

}

unsafe fn link(vs: &str, fs: &str) -> Option<u32> {
    unsafe fn compile(typ: u32, src: &str) -> Option<u32> {
        let s = glCreateShader(typ);
        let c = CString::new(src).unwrap();
        let ptr = c.as_ptr();
        glShaderSource(s, 1, &ptr, std::ptr::null());
        glCompileShader(s);
        let mut ok = 0;
        glGetShaderiv(s, 0x8B81, &mut ok);
        if ok == 0 {
            return None;
        }
        Some(s)
    }
    let v = compile(0x8B31, vs)?;
    let f = compile(0x8B30, fs)?;
    let p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glLinkProgram(p);
    let mut ok = 0;
    glGetProgramiv(p, 0x8B82, &mut ok);
    if ok == 0 {
        return None;
    }
    Some(p)
}
