use std::{
    alloc::{self, Layout},
    borrow::Cow,
    mem::MaybeUninit,
    ptr::{self, addr_of, addr_of_mut, null_mut, NonNull},
};

use crate::{
    chunk::{InstructionDebug, Opcode},
    native_fn::NativeFnKind,
    obj::{Obj, ObjClosure, ObjFunction, ObjKind, ObjList, ObjNative, ObjString, ObjUpvalue},
    table::{ObjHash, Table},
    value::Value,
};

pub type InterpretResult<T> = Result<T, InterpretError>;

#[derive(Debug)]
pub enum InterpretError {
    RuntimeError,
    CompileError,
}

#[derive(Debug, Copy, Clone)]
pub struct CallFrame {
    /// PERF: Instruction pointer is faster to dereference than index
    /// instruction index of caller, this is used for returns, function will jump back to this index
    /// and resume from there
    pub instr_offset: u32,

    /// PERF: pointer is faster to dereference than index
    /// index into VM's value stack at the first slot this function can use
    pub slots_offset: u32,

    pub closure: NonNull<ObjClosure>,
}

impl CallFrame {
    #[inline]
    fn function(&self) -> &ObjFunction {
        unsafe { self.closure.as_ref().function.as_ref() }
    }
    #[inline]
    fn closure(&self) -> &ObjClosure {
        unsafe { self.closure.as_ref() }
    }
}

pub const U8_COUNT: usize = (u8::MAX) as usize + 1; // 256
const FRAMES_MAX: usize = 64;
const STACK_MAX: usize = 64 * U8_COUNT;

pub struct VM {
    pub stack: [MaybeUninit<Value>; STACK_MAX],
    pub stack_top: u32,
    /// linked list of open upvalues for the top-most stack,
    /// this points to the top-most open upvalue
    /// (so last in this list is the first open upvalue on the stack)
    pub open_upvalues: *mut ObjUpvalue,

    pub call_frames: [MaybeUninit<CallFrame>; FRAMES_MAX],
    pub call_frame_count: u32,

    pub obj_list: ObjList,
    pub globals: Table,
    pub interned_strings: Table,
}

impl VM {
    pub fn new(function: NonNull<ObjFunction>, mut obj_list: ObjList, strings: Table) -> Self {
        // im pretty sure we might need to push/pop function here
        // becuase of gc but will just wait until that chapter
        let closure = ObjClosure::new(&mut obj_list, function);
        let mut call_frames = [MaybeUninit::uninit(); FRAMES_MAX];
        call_frames[0] = MaybeUninit::new(CallFrame {
            instr_offset: 0,
            slots_offset: 0,
            closure,
        });

        let mut stack = [MaybeUninit::uninit(); STACK_MAX];
        stack[0] = MaybeUninit::new(Value::Obj(closure.cast()));

        let mut this = Self {
            open_upvalues: null_mut(),
            obj_list,
            globals: Table::new(),
            interned_strings: strings,
            stack,
            stack_top: 1,
            call_frames,
            call_frame_count: 1,
        };

        this.define_native("clock", NativeFnKind::Clock);
        this.define_native("__dummy", NativeFnKind::Dummy);

        this
    }

    pub fn get_string(&mut self, string: &str) -> NonNull<ObjString> {
        ObjString::copy_string(&mut self.interned_strings, &mut self.obj_list, string)
    }

    #[inline]
    fn push(&mut self, val: Value) {
        self.stack[self.stack_top as usize] = MaybeUninit::new(val);
        self.stack_top += 1;
    }

    #[inline]
    fn pop(&mut self) -> Value {
        self.stack_top -= 1;
        unsafe { MaybeUninit::assume_init(self.stack[self.stack_top as usize]) }
    }

    #[inline]
    fn binary_op<F: FnOnce(Value, Value) -> Value>(&mut self, f: F) -> InterpretResult<()> {
        if !matches!(self.peek(0), Value::Number(_)) || !matches!(self.peek(1), Value::Number(_)) {
            self.runtime_error("Operands must be two numbers or two strings.".into());
            return Err(InterpretError::RuntimeError);
        }

        let b = self.pop();
        let a = self.pop();
        self.push(f(a, b));

        Ok(())
    }

    #[inline]
    fn reset_stack(&mut self) {
        self.stack_top = 0;
        self.call_frame_count = 0;
        self.open_upvalues = null_mut();
    }

    fn runtime_error<'a>(&mut self, err: Cow<'a, str>) {
        eprintln!("{}", err);

        let frame = self.top_call_frame();
        let instr_idx = frame.instr_offset;
        let line = frame.function().chunk.lines[instr_idx as usize];

        eprintln!("[line {}] in script", line);

        for frame in self
            .call_frames
            .iter()
            .take(self.call_frame_count as usize - 1)
            .rev()
        {
            unsafe {
                let frame = frame.assume_init();
                let function = frame.function();
                let instruction = frame.instr_offset;
                eprintln!(
                    "[line {}] in {}",
                    function.chunk.lines[instruction as usize],
                    match function.name.as_ref() {
                        Some(name) => name.as_str(),
                        None => "script",
                    }
                )
            }
        }

        self.reset_stack();
    }

    fn peek(&self, distance: u32) -> Value {
        unsafe { self.stack[self.stack_top as usize - 1 - distance as usize].assume_init() }
    }

    fn concatenate(&mut self) {
        let b = self.pop();
        let a = self.pop();

        let b = b.as_obj_str().unwrap();
        let a = a.as_obj_str().unwrap();

        let new_len = a.len + b.len;

        let obj_str = if new_len == 0 {
            ObjString::alloc_str(
                &mut self.interned_strings,
                &mut self.obj_list,
                NonNull::dangling(),
                0,
                ObjHash::EMPTY_STR_HASH,
            )
        } else {
            let layout = Layout::array::<u8>(new_len as usize).unwrap();
            let chars = unsafe { alloc::alloc(layout) };

            unsafe {
                ptr::copy_nonoverlapping(a.chars.as_ptr(), chars, a.len as usize);
                ptr::copy_nonoverlapping(
                    b.chars.as_ptr(),
                    chars.offset(a.len as isize),
                    b.len as usize,
                );
            }

            ObjString::take_string(
                &mut self.interned_strings,
                &mut self.obj_list,
                chars,
                new_len,
            )
        };

        self.push(Value::Obj(obj_str.cast()))
    }

    fn call(&mut self, closure: NonNull<ObjClosure>, arg_count: u8) -> bool {
        let arity = unsafe { closure.as_ref().function.as_ref().arity };
        if arg_count != arity {
            self.runtime_error(
                format!("Expected {} arguments but got {}.", arg_count, arity).into(),
            );
            return false;
        }

        if self.call_frame_count as usize == FRAMES_MAX {
            self.runtime_error("Stack overflow.".into());
            return false;
        }

        self.next_call_frame(closure, arg_count);

        true
    }

    fn define_native(&mut self, name: &str, native_fn_kind: NativeFnKind) {
        let name = Value::Obj(
            ObjString::copy_string(&mut self.interned_strings, &mut self.obj_list, name).cast(),
        );

        let native_fn = Value::Obj(ObjNative::new(&mut self.obj_list, native_fn_kind).cast());

        self.push(name);
        self.push(native_fn);

        unsafe {
            self.globals.set(
                self.stack[self.stack_top as usize - 2]
                    .assume_init()
                    .as_obj_str_ptr()
                    .unwrap(),
                self.stack[self.stack_top as usize - 1].assume_init(),
            );
        }

        self.pop();
        self.pop();
    }

    fn call_value(&mut self, callee: Value, arg_count: u8) -> bool {
        match callee {
            Value::Obj(obj) => {
                let kind = unsafe { obj.as_ref().kind };
                match kind {
                    ObjKind::Closure => return self.call(obj.cast(), arg_count),
                    ObjKind::Native => {
                        let native: NonNull<ObjNative> = obj.cast();
                        let stack_begin = self.stack_top as usize - arg_count as usize;
                        let values = &self.stack[stack_begin..];
                        let result =
                            unsafe { native.as_ref().function.call(std::mem::transmute(values)) };

                        self.stack_top -= arg_count as u32 + 1;
                        self.push(result);
                        return true;
                    }
                    _ => (),
                }
            }
            _ => {}
        }

        self.runtime_error("Can only call functions and classes.".into());
        false
    }

    fn capture_upvalue(&mut self, offset: u8) -> NonNull<ObjUpvalue> {
        let slot = self.top_call_frame().slots_offset;
        let local = NonNull::new(self.stack[slot as usize + offset as usize].as_mut_ptr()).unwrap();

        unsafe {
            let mut prev_upvalue: *mut ObjUpvalue = null_mut();
            let mut upvalue: *mut ObjUpvalue = self.open_upvalues;

            // the list is sorted in stack order (first upvalue location points to top-most local)
            // so we keep iterating skipping upvalues whose location is above our desired, hence the
            // `(*upvalue).location > local`
            while !upvalue.is_null() && (*upvalue).location > local {
                prev_upvalue = upvalue;
                upvalue = (*upvalue).next;
            }

            match NonNull::new(upvalue) {
                // found a captured upvalue, reuse it
                Some(upvalue_nonnull) if (*upvalue).location == local => return upvalue_nonnull,
                _ => (),
            }

            let created_upvalue = ObjUpvalue::new(&mut self.obj_list, local);
            (*created_upvalue.as_ptr()).next = upvalue;

            if prev_upvalue.is_null() {
                self.open_upvalues = created_upvalue.as_ptr();
            } else {
                (*prev_upvalue).next = created_upvalue.as_ptr();
            }

            return created_upvalue;
        }
    }

    fn read_closure_upvalues(&mut self, mut closure: NonNull<ObjClosure>) {
        unsafe {
            let closure = closure.as_mut();
            for i in 0..closure.upvalue_count {
                let byte = self.read_byte();
                let is_local = byte == 1;
                let index = self.read_byte();

                let upvalue = if is_local {
                    self.capture_upvalue(index).as_ptr()
                } else {
                    // at this point we haven't switched call frame to closure yet, so we
                    // read from current call frame which is actually closure's surrounding call frame
                    *(self
                        .top_call_frame()
                        .closure
                        .as_ref()
                        .upvalues
                        .as_ptr()
                        .offset(index as isize))
                };

                (*closure.upvalues.as_ptr().offset(i as isize)) = upvalue;
            }
        }
    }

    /// Closes upvalues of a scope
    fn close_upvalues(&mut self, last_stack_offset: u32) {
        unsafe {
            let last = NonNull::new(self.stack[last_stack_offset as usize].as_mut_ptr()).unwrap();
            while let Some(upvalue) = NonNull::new(self.open_upvalues) {
                if (*upvalue.as_ptr()).location < last {
                    return;
                }

                let upvalue_ptr = upvalue.as_ptr();
                (*upvalue_ptr).closed = *((*upvalue_ptr).location.as_ptr());
                (*upvalue_ptr).location =
                    NonNull::new(addr_of_mut!((*upvalue_ptr).closed)).unwrap();
                self.open_upvalues = (*upvalue_ptr).next;
            }
        }
    }

    pub fn run(&mut self) -> InterpretResult<()> {
        loop {
            #[cfg(debug_assertions)]
            {
                // Debug stack
                for slot in self.stack.iter().take(self.stack_top as usize) {
                    let value: &Value = unsafe { slot.assume_init_ref() };
                    println!("          [ {:?} ]", value);
                }

                // Debug instruction
                let frame = self.top_call_frame();
                let mut duplicate_instruction_index = frame.instr_offset as usize;
                let line = frame.function().chunk.lines[duplicate_instruction_index];
                let inner = frame
                    .function()
                    .chunk
                    .disassemble_instruction(&mut duplicate_instruction_index);
                println!("{:?}", inner.map(|inner| InstructionDebug { line, inner }));
            }

            let byte = self.read_byte();

            match Opcode::from_u8(byte) {
                Some(Opcode::CloseUpvalue) => {
                    self.close_upvalues(self.stack_top - 1);
                    self.pop();
                }
                Some(Opcode::GetUpvalue) => {
                    let slot = self.read_byte();
                    let val = unsafe {
                        *(self
                            .top_call_frame()
                            .closure
                            .as_ref()
                            .upvalue_at_slot(slot as usize)
                            .unwrap()
                            .as_ref()
                            .location
                            .as_ptr())
                    };

                    self.push(val);
                }
                Some(Opcode::SetUpvalue) => {
                    let slot = self.read_byte();
                    let val = self.peek(0);
                    unsafe {
                        let loc_ptr = self
                            .top_call_frame()
                            .closure()
                            .upvalue_at_slot(slot as usize)
                            .unwrap()
                            .as_mut()
                            .location
                            .as_ptr();

                        (*loc_ptr) = val;
                    }
                }
                Some(Opcode::Closure) => {
                    let function = self.read_constant().as_fn_ptr().unwrap();
                    let closure = ObjClosure::new(&mut self.obj_list, function);
                    self.read_closure_upvalues(closure);
                    self.push(Value::Obj(closure.cast()));
                }
                Some(Opcode::Call) => {
                    let arg_count = self.read_byte();
                    if !self.call_value(self.peek(arg_count as u32), arg_count) {
                        return Err(InterpretError::RuntimeError);
                    }
                }
                Some(Opcode::Loop) => {
                    let offset = self.read_u16();
                    self.top_call_frame_mut().instr_offset -= offset as u32;
                }
                Some(Opcode::Jump) => {
                    let offset = self.read_u16();
                    self.top_call_frame_mut().instr_offset += offset as u32;
                }
                Some(Opcode::JumpIfFalse) => {
                    let offset = self.read_u16();
                    if self.peek(0).is_falsey() {
                        self.top_call_frame_mut().instr_offset += offset as u32;
                    }
                }
                Some(Opcode::GetLocal) => {
                    let slot = self.read_byte();
                    let slot_offset = self.top_call_frame().slots_offset;
                    let val =
                        unsafe { self.stack[slot as usize + slot_offset as usize].assume_init() };

                    self.push(val);
                }
                Some(Opcode::SetLocal) => {
                    let slot = self.read_byte();
                    let slot_offset = self.top_call_frame().slots_offset;
                    self.stack[slot as usize + slot_offset as usize] =
                        MaybeUninit::new(self.peek(0));
                }
                Some(Opcode::SetGlobal) => {
                    let name = self
                        .read_constant()
                        .as_obj_str_ptr()
                        .expect("Expect string constant for global variable name.");

                    let new_val = self.peek(0);
                    println!("{} = {:?}", unsafe { name.as_ref() }.as_str(), self.peek(0));

                    if self.globals.set(name, new_val) {
                        self.globals.delete(name);
                        self.runtime_error(
                            format!("Undefined variable: {}", unsafe { name.as_ref().as_str() })
                                .into(),
                        );

                        return Err(InterpretError::RuntimeError);
                    }
                }
                Some(Opcode::GetGlobal) => {
                    let name = self
                        .read_constant()
                        .as_obj_str_ptr()
                        .expect("Expect string constant for global variable name.");

                    let val = match self.globals.get(name) {
                        Some(global) => global,
                        None => {
                            self.runtime_error(
                                format!("Undefined variable: {}", unsafe {
                                    name.as_ref().as_str()
                                })
                                .into(),
                            );

                            return Err(InterpretError::RuntimeError);
                        }
                    };

                    self.push(val);
                }
                Some(Opcode::DefineGlobal) => {
                    let name = self
                        .read_constant()
                        .as_obj_str_ptr()
                        .expect("Expect string constant for global variable name.");

                    self.globals.set(name, self.peek(0));
                    self.pop();
                }
                Some(Opcode::Nil) => {
                    self.push(Value::Nil);
                }
                Some(Opcode::Pop) => {
                    self.pop();
                }
                Some(Opcode::Print) => {
                    let value = self.pop();
                    println!("{:?}", value);
                }
                Some(Opcode::Equal) => {
                    let b = self.pop();
                    let a = self.pop();

                    self.push(Value::Bool(a == b))
                }
                Some(Opcode::Not) => {
                    let top = self.pop();
                    self.push(Value::Bool(top.is_falsey()))
                }
                Some(Opcode::Negate) => {
                    if !matches!(self.peek(0), Value::Bool(_) | Value::Number(_)) {
                        self.runtime_error("Operand must be a number.".into());
                        return Err(InterpretError::RuntimeError);
                    }

                    let negated = -self.pop();
                    self.push(negated)
                }
                Some(Opcode::Return) => {
                    if self.call_frame_count == 1 {
                        self.pop();
                        return Ok(());
                    }

                    let result = self.pop();
                    let slots_offset = self.top_call_frame().slots_offset;
                    self.close_upvalues(slots_offset);

                    self.call_frame_count -= 1;

                    self.stack_top = self.top_call_frame().slots_offset;
                    self.push(result);
                }
                Some(Opcode::Constant) => {
                    let constant = self.read_constant();
                    self.push(constant);
                }
                Some(Opcode::Subtract) => self.binary_op(std::ops::Sub::sub)?,
                Some(Opcode::Multiply) => self.binary_op(std::ops::Mul::mul)?,
                Some(Opcode::Divide) => self.binary_op(std::ops::Div::div)?,
                Some(Opcode::Greater) => self.binary_op(Value::gt_owned)?,
                Some(Opcode::Less) => self.binary_op(Value::lt_owned)?,
                Some(Opcode::Add) => {
                    if self.peek(0).is_str() && self.peek(1).is_str() {
                        self.concatenate();
                    } else {
                        self.binary_op(std::ops::Add::add)?
                    }
                }
                otherwise => panic!("Unknown opcode {:?}", otherwise),
            }
        }
    }

    #[inline]
    fn read_byte(&mut self) -> u8 {
        let frame =
            unsafe { self.call_frames[self.call_frame_count as usize - 1].assume_init_mut() };

        let ret = frame.function().chunk.code[frame.instr_offset as usize];
        frame.instr_offset += 1;

        ret
    }

    #[inline]
    fn next_call_frame(&mut self, closure: NonNull<ObjClosure>, arg_count: u8) {
        let call_frame = self.call_frames[self.call_frame_count as usize].as_mut_ptr();
        self.call_frame_count += 1;
        unsafe {
            (*call_frame).instr_offset = 0;
            (*call_frame).slots_offset = self.stack_top - arg_count as u32 - 1;
            (*call_frame).closure = closure;
        }
    }

    #[inline]
    fn top_call_frame_ptr(&mut self) -> NonNull<CallFrame> {
        unsafe {
            NonNull::new_unchecked(
                self.call_frames[self.call_frame_count as usize - 1].as_mut_ptr(),
            )
        }
    }
    #[inline]
    fn top_call_frame_mut(&mut self) -> &mut CallFrame {
        unsafe { self.call_frames[self.call_frame_count as usize - 1].assume_init_mut() }
    }
    #[inline]
    fn top_call_frame(&mut self) -> &CallFrame {
        unsafe { self.call_frames[self.call_frame_count as usize - 1].assume_init_ref() }
    }

    #[inline]
    fn read_u16(&mut self) -> u16 {
        let frame = self.top_call_frame_mut();

        let (byte1, byte2) = (
            frame.function().chunk[frame.instr_offset as usize],
            frame.function().chunk[frame.instr_offset as usize + 1],
        );

        frame.instr_offset += 2;

        ((byte1 as u16) << 8) | (byte2 as u16)
    }

    #[inline]
    fn read_constant(&mut self) -> Value {
        let idx = self.read_byte();
        self.top_call_frame().function().chunk.constants[idx as usize]
    }

    fn free_objects(&mut self) {
        for obj in self.obj_list.iter_mut() {
            Obj::free(*obj)
        }
    }
}

impl Drop for VM {
    fn drop(&mut self) {
        self.free_objects();
        Table::free(&mut self.interned_strings);
        Table::free(&mut self.globals);
    }
}
