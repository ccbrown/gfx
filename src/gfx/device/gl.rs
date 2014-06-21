extern crate gl;
extern crate libc;

use std;
use platform::GlProvider;

pub type Buffer         = gl::types::GLuint;
pub type ArrayBuffer    = gl::types::GLuint;
pub type Shader         = gl::types::GLuint;
pub type Program        = gl::types::GLuint;

pub struct Device;


impl Device {
    pub fn new(provider: &GlProvider) -> Device {
        gl::load_with(|s| provider.get_proc_address(s));
        Device
    }

    fn check(&self) {
        assert_eq!(gl::GetError(), gl::NO_ERROR);
    }

    pub fn clear(&self, color: &[f32]) {
        gl::ClearColor(color[0], color[1], color[2], color[3]);
        gl::Clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
    }

    pub fn create_buffer<T>(&self, data: &[T]) -> Buffer {
        let mut name = 0 as Buffer;
        unsafe{
            gl::GenBuffers(1, &mut name);
        }
        gl::BindBuffer(gl::ARRAY_BUFFER, name);
        info!("\tCreated buffer {}", name);
        let size = (data.len() * std::mem::size_of::<T>()) as gl::types::GLsizeiptr;
        let raw = data.as_ptr() as *gl::types::GLvoid;
        unsafe{
            gl::BufferData(gl::ARRAY_BUFFER, size, raw, gl::STATIC_DRAW);
        }
        name
    }

    pub fn create_array_buffer(&self) -> ArrayBuffer {
        let mut name = 0 as ArrayBuffer;
        unsafe{
            gl::GenVertexArrays(1, &mut name);
        }
        info!("\tCreated array buffer {}", name);
        name
    }

    pub fn create_shader(&self, kind: char, data: &[u8]) -> Shader {
        let target = match kind {
            'v' => gl::VERTEX_SHADER,
            'g' => gl::GEOMETRY_SHADER,
            'f' => gl::FRAGMENT_SHADER,
            _   => fail!("Unknown shader kind: {}", kind)
        };
        let name = gl::CreateShader(target);
        let mut length = data.len() as gl::types::GLint;
        unsafe {
            gl::ShaderSource(name, 1, &(data.as_ptr() as *gl::types::GLchar), &length);
        }
        gl::CompileShader(name);
        info!("\tCompiled shader {}", name);
        // get info message
        let mut status = 0 as gl::types::GLint;
        length = 0;
        unsafe {
            gl::GetShaderiv(name, gl::COMPILE_STATUS,  &mut status);
            gl::GetShaderiv(name, gl::INFO_LOG_LENGTH, &mut length);
        }
        let mut info = String::with_capacity(length as uint);
        info.grow(length as uint, 0u8 as char);
        unsafe {
            gl::GetShaderInfoLog(name, length, &mut length,
                info.as_slice().as_ptr() as *mut gl::types::GLchar);
        }
        info.truncate(length as uint);
        if status == 0  {
            error!("Failed shader code:\n{}\n", std::str::from_utf8(data).unwrap());
            fail!("GLSL: {}", info);
        }
        name
    }

    fn query_program_int(&self, prog: Program, query: gl::types::GLenum) -> gl::types::GLint {
        let mut ret = 0 as gl::types::GLint;
        unsafe {
            gl::GetProgramiv(prog, query, &mut ret);
        }
        ret
    }

    pub fn create_program(&self, shaders: &[Shader]) -> Program {
        let name = gl::CreateProgram();
        for &sh in shaders.iter() {
            gl::AttachShader(name, sh);
        }
        gl::LinkProgram(name);
        info!("\tLinked program {}", name);
        //info!("\tLinked program {} from objects {}", h, shaders);
        // get info message
        let status      = self.query_program_int(name, gl::LINK_STATUS);
        let mut length  = self.query_program_int(name, gl::INFO_LOG_LENGTH);
        let mut info = String::with_capacity(length as uint);
        info.grow(length as uint, 0u8 as char);
        unsafe {
            gl::GetProgramInfoLog(name, length, &mut length,
                info.as_slice().as_ptr() as *mut gl::types::GLchar);
        }
        info.truncate(length as uint);
        if status == 0  {
            error!("GL error {}", gl::GetError());
            fail!("GLSL program error: {}", info)
        }
        name
    }

    pub fn draw(&self, buffer: Buffer, array_buffer: ArrayBuffer, program: Program, count: uint) {
        gl::PolygonMode(gl::FRONT_AND_BACK, gl::FILL);
        gl::Disable(gl::CULL_FACE);
        gl::Disable(gl::DEPTH_TEST);
        gl::Disable(gl::STENCIL_TEST);
        gl::UseProgram(program);
        gl::BindVertexArray(array_buffer);
        gl::BindBuffer(gl::ARRAY_BUFFER, buffer);
        unsafe{
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 8, std::ptr::null());
        }
        gl::EnableVertexAttribArray(0);
        gl::DrawArrays(gl::TRIANGLES, 0, count as gl::types::GLsizei);
    }
}
