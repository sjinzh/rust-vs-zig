#![feature(allocator_api)]
#![feature(slice_ptr_get)]
#![feature(let_chains)]

pub mod chunk;
pub mod compile;
pub mod mem;
pub mod native_fn;
pub mod obj;
pub mod table;
pub mod value;
pub mod vm;

use std::{io::BufRead, mem::MaybeUninit, path::Path};

use compile::Parser;
use mem::Mem;

use vm::{InterpretError, InterpretResult, ValueStack, STACK_MAX};

use crate::vm::VM;

fn main() {
    // run_file("./test.lox")

    let mut args = std::env::args();
    let _ = args.next();

    match args.len() {
        0 => {
            repl();
        }
        1 => {
            run_file(args.next().unwrap());
        }
        _ => panic!(),
    }
}

fn repl() {
    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();

    for line in lines {
        let line = line.unwrap();
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        interpret(&mut stack, &line).unwrap();
    }
}

fn run_file<P: AsRef<Path>>(path: P) {
    let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
    let string = std::fs::read_to_string(path).unwrap();
    interpret(&mut stack, &string).unwrap();
}

fn interpret<'a>(stack: &'a mut ValueStack, src: &str) -> InterpretResult<VM<'a>> {
    let mut mem = Mem::new();

    let function = {
        let mut parser = Parser::new(src, &mut mem);
        if !parser.compile() {
            return Err(InterpretError::CompileError);
        }
        parser.compiler.function
    };

    // let stack = Box::leak(Box::new([MaybeUninit::uninit(); STACK_MAX]));
    let mut vm = VM::new(stack, mem, function);

    vm.run().map(|_| vm)
}

#[cfg(test)]
mod test {

    use std::mem::MaybeUninit;

    use crate::{
        compile::Token,
        interpret,
        mem::Mem,
        table::Table,
        value::Value,
        vm::{ValueStack, STACK_MAX},
    };

    #[test]
    fn upvalue_closed() {
        let src = r#"
    fun makeClosure() {
      var a = 1;
      fun f() {
        a = a + 1;
        return a;
      }
      return f;
    }

    var closure = makeClosure();
    var first = closure();
    var anotherClosure = makeClosure();
    var second = anotherClosure();
    var third = closure();"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let first_str = vm.get_string("first");
        let second_str = vm.get_string("second");
        let third_str = vm.get_string("third");

        let value1 = vm.mem.globals.get(first_str);
        let value2 = vm.mem.globals.get(second_str);
        let value3 = vm.mem.globals.get(third_str);

        assert_eq!(value1, Some(Value::Number(2.0)));
        assert_eq!(value2, Some(Value::Number(2.0)));
        assert_eq!(value3, Some(Value::Number(3.0)));
    }

    #[test]
    fn set_immediate_upvalue() {
        let src = r#"
    fun outer() {
      var x = 420;
      fun inner() {
        x = x + 1;
        return x;
      }
      return inner();
    }
    var value = outer();"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();
        let value_str = vm.get_string("value");

        let value = vm.mem.globals.get(value_str);

        assert_eq!(value, Some(Value::Number(421.0)));
    }

    #[test]
    fn immediate_upvalue() {
        let src = r#"
var result = "nothing";
fun outer() {
  var x = 420;
  fun inner() {
    result = x;
  }
  inner();
}
outer();"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();
        let result_str = vm.get_string("result");
        let value = vm.mem.globals.get(result_str);

        assert_eq!(value, Some(Value::Number(420.0)));
    }

    #[test]
    fn call_native_fn() {
        let src = r#"
        var num = __dummy();"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();
        let num_str = vm.get_string("num");
        let value = vm.mem.globals.get(num_str);
        assert_eq!(value, Some(Value::Number(420.0)));
    }

    #[test]
    fn call_fn() {
        let src = r#"
            fun add420(num) {
              return num + 420;
            }

            fun add69(num) {
              return num + 69;
            }

            var num = add420(1);
            num = add69(num);
            num = add420(num);"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();
        let num_str = vm.get_string("num");
        let value = vm.mem.globals.get(num_str);
        assert_eq!(value, Some(Value::Number(910.0)));
    }

    #[test]
    fn print_fn() {
        let src = r#"
            fun bigNoob() {
              print "OH YEAH";
            }

            print bigNoob;"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let _vm = interpret(&mut stack, src).unwrap();
    }

    #[test]
    fn if_stmt() {
        let src = r#"
            var noob = 420;
            if (420 > 69) { noob = "NICE"; } else { noob = "NOT NICE"; }
    "#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let noob = vm.get_string("noob");
        let top = vm.mem.globals.get(noob);
        assert_eq!(top.unwrap().as_str(), Some("NICE"));
    }

    #[test]
    fn if_else_stmt() {
        let src = r#"
            var noob = 420;
            if (69 > 420) { noob = "wtf"; } else { noob = "NICE"; }
    "#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let noob = vm.get_string("noob");
        let top = vm.mem.globals.get(noob);
        assert_eq!(top.unwrap().as_str(), Some("NICE"));
    }

    #[test]
    fn while_loop() {
        let src = r#"
            var noob = 0;
            while (noob < 10) {
              noob = noob + 1;
            }
    "#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let noob = vm.get_string("noob");
        let top = vm.mem.globals.get(noob);
        assert_eq!(top, Some(Value::Number(10.0)));
    }

    #[test]
    fn for_loop() {
        let src = r#"
            var noob = 420;
            for (var x = 0; x < 10; x = x + 1) {
              noob = x;
            }
    "#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let _vm = interpret(&mut stack, src).unwrap();

        // let noob = vm.get_string("global");
        // let top = vm.mem.globals.get(noob);
        // assert_eq!(top.unwrap().as_str(), Some("NICE"));
    }

    #[test]
    fn locals() {
        let src = r#"
            var global = 420;
            { var x = "HELLO"; x = "NICE"; global = x; }
    "#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let noob = vm.get_string("global");
        let top = vm.mem.globals.get(noob);
        assert_eq!(top.unwrap().as_str(), Some("NICE"));
    }

    #[test]
    fn string() {
        let src = r#"var noob = "hello" + " sir" + " sir";"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let mut vm = interpret(&mut stack, src).unwrap();

        let noob = vm.get_string("noob");
        let top = vm.mem.globals.get(noob);
        assert_eq!(top.unwrap().as_str(), Some("hello sir sir"));
    }

    #[test]
    fn print() {
        let src = r#"print 1 + 2;"#;
        let mut stack: ValueStack = [MaybeUninit::uninit(); STACK_MAX];
        let _vm = interpret(&mut stack, src).unwrap();
    }

    #[test]
    fn table() {
        let mut mem = Mem::new();
        let mut table = Table::new();

        let key = mem.copy_string("bagel");
        assert_eq!(table.set(key, Value::Number(420.0)), true);
        assert_eq!(table.set(key, Value::Number(69.0)), false);
        assert_eq!(table.get(key), Some(Value::Number(69.0)));
        assert_eq!(table.delete(key), true);
        assert_eq!(table.delete(key), false);

        Table::free(&mut table);
    }

    #[test]
    fn ohshit() {
        // let bytes = [0, 1, 2, 3];

        println!("NOOB: {:?}", std::mem::size_of::<Token>());

        let values = [0, 1, 2, 3, 4, 5];
        println!(
            "NICE: {:?}",
            values.iter().take(3).rev().collect::<Vec<_>>()
        );

        // 0
        // 1
        // 2 ---
        // 3 ---
        // 4
        // 5
        //
        // 6
        // -2 to adjust for the 2 bytes for the jump offset
        let mut chunk = [0, 1, 2, 3, 4, 5];
        let offset = 2;
        let jump = chunk.len() as u32 - offset - 2;

        chunk[offset as usize] = (jump >> 8) as u8;
        chunk[offset as usize + 1] = jump as u8;

        let val = ((chunk[offset as usize] as u16) << 8) | (chunk[offset as usize + 1] as u16);

        println!("{jump} NOOB: {chunk:?} JUMP: {val} {}", 2u16);
    }
}

fn _f(_a: i32, _b: i32) -> i32 {
    420
}

fn _noob() {
    let _noob = _f(_f(1, 2), _f(3, 4));
}