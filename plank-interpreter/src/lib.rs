extern crate plank_ir;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use plank_ir::{ir, Program};


#[derive(Debug)]
pub enum Error {
    BadDeref,
    DivisionByZero,
    MissingSymbol(String),
    Io(io::Error),
}

impl ::std::convert::From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

pub fn run_program<R: Read, W: Write>(program: &Program, input: R, output: W) -> Result<(), Error> {
    plank_ir::validate_ir(program);
    Vm::new(program, input, output)?.run()
}

#[derive(Debug, Copy, Clone)]
enum Value {
    AddressRange(u32, u32),
    FromAddress(u32),
    Byte(u8),
    Word(u16),
    DoubleWord(u32),
}

struct StackFrame<'a> {
    stack_start: usize,
    function: &'a ir::Function,
    registers: HashMap<ir::Reg, u32>,
    current_block: ir::BlockId,
    current_op: usize,
    return_address: Option<u32>,
}

struct Vm<'a, R, W> {
    input: R,
    output: W,
    program: &'a Program,
    memory: Vec<u8>,
    frames: Vec<StackFrame<'a>>,
    current_frame: StackFrame<'a>,
}

impl<'a, R: Read, W: Write> Vm<'a, R, W> {
    fn new(program: &'a Program, input: R, output: W) -> Result<Self, Error> {
        let main_symbol = ir::Symbol("main".into());
        let main = match program.functions.get(&main_symbol) {
            Some(f) => f,
            None => return Err(Error::MissingSymbol("main".into())),
        };
        let block = match main.start_block {
            Some(block) => block,
            None => return Err(Error::MissingSymbol("main".into())),
        };
        let main_frame = StackFrame {
            stack_start: 4,
            function: main,
            registers: HashMap::new(),
            current_block: block,
            current_op: 0,
            return_address: Some(0),
        };
        let mut vm = Vm {
            input,
            output,
            program,
            memory: vec![0, 0, 0, 0],
            frames: Vec::new(),
            current_frame: main_frame,
        };
        let regs = vm.allocate_registers(&vm.current_frame.function.registers);
        vm.current_frame.registers = regs;
        Ok(vm)
    }

    fn current_block(&self) -> &'a ir::Block {
        &self.current_frame.function.blocks[&self.current_frame.current_block]
    }

    fn allocate_registers(&mut self, registers: &HashMap<ir::Reg, ir::Layout>) -> HashMap<ir::Reg, u32> {
        let mut result = HashMap::new();
        for (&reg, &layout) in registers {
            let at = self.memory.len() as u32;
            for _ in 0..layout.size {
                self.memory.push(0);
            }
            result.insert(reg, at);
        }
        result
    }

    fn load_8bit(&self, val: &ir::Value) -> u8 {
        match *val {
            ir::Value::Int(i, ir::Size::Bit8) => i as u8,
            ir::Value::Reg(reg) => {
                let reg_at = self.current_frame.registers[&reg] as usize;
                if reg_at < self.memory.len() {
                    self.memory[reg_at]
                } else {
                    panic!("register out of bounds")
                }
            }
            _ => panic!("bad 8 bit value"),
        }
    }

    fn load_16bit(&self, val: &ir::Value) -> u16 {
        match *val {
            ir::Value::Int(i, ir::Size::Bit16) => i as u16,
            ir::Value::Reg(reg) => {
                let reg_at = self.current_frame.registers[&reg] as usize;
                if reg_at + 4 <= self.memory.len() {
                    let b1 = self.memory[reg_at + 0] as u16;
                    let b2 = self.memory[reg_at + 1] as u16;
                    (b2 << 8) | (b1 << 0)
                } else {
                    panic!("register out of bounds")
                }
            }
            ir::Value::Bytes(_) => unimplemented!("string literals"),
            ir::Value::Symbol(_) => unimplemented!("virtual calls"),
            _ => panic!("bad 16 bit value"),
        }
    }

    fn load_32bit(&self, val: &ir::Value) -> u32 {
        match *val {
            ir::Value::Int(i, ir::Size::Bit32) => i as u32,
            ir::Value::Reg(reg) => {
                let reg_at = self.current_frame.registers[&reg] as usize;
                if reg_at + 4 <= self.memory.len() {
                    let b1 = self.memory[reg_at + 0] as u32;
                    let b2 = self.memory[reg_at + 1] as u32;
                    let b3 = self.memory[reg_at + 2] as u32;
                    let b4 = self.memory[reg_at + 3] as u32;
                    (b4 << 24) | (b3 << 16) | (b2 << 8) | (b1 << 0)
                } else {
                    panic!("register out of bounds")
                }
            }
            ir::Value::Bytes(_) => unimplemented!("string literals"),
            ir::Value::Symbol(_) => unimplemented!("virtual calls"),
            _ => panic!("bad 32 bit value"),
        }
    }

    fn read_value(&self, val: &ir::Value) -> Value {
        match *val {
            ir::Value::Bytes(_) => unimplemented!("string literals"),
            ir::Value::Int(i, ir::Size::Bit8) => Value::Byte(i as u8),
            ir::Value::Int(i, ir::Size::Bit16) => Value::Word(i as u16),
            ir::Value::Int(i, ir::Size::Bit32) => Value::DoubleWord(i as u32),
            ir::Value::Reg(reg) => {
                let (at, size) = self.register_address(reg);
                Value::AddressRange(at, size)
            }
            ir::Value::Symbol(_) => unimplemented!("virtual calls")
        }
    }

    fn register_address(&self, reg: ir::Reg) -> (u32, u32) {
        let at = self.current_frame.registers[&reg];
        let size = self.current_frame.function.registers[&reg].size;
        (at, size)
    }

    fn mem_copy(&mut self, from: u32, to: u32, len: u32) {
        if from == to {
            return;
        }
        if from < to && from + len > to {
            panic!("copy overlap");
        }
        if to < from && to + len > from {
            panic!("copy overlap");
        }
        for i in 0..len {
            self.memory[(to + i) as usize] = self.memory[(from + i) as usize];
        }
    }

    fn mem_cmp(&self, a: u32, b: u32, len: u32) -> bool {
        for i in 0..len {
            if self.memory[(a + i) as usize] != self.memory[(b + i) as usize] {
                return false;
            }
        }
        true
    }

    fn compare_values(&self, a: Value, b: Value) -> bool {
        match (a, b) {
            (Value::AddressRange(a, al), Value::AddressRange(b, bl)) => {
                assert_eq!(al, bl);
                self.mem_cmp(a, b, al)
            }
            (Value::AddressRange(a, al), Value::Byte(b)) |
            (Value::Byte(b), Value::AddressRange(a, al)) => {
                assert_eq!(al, 1);
                self.memory[a as usize] == b
            }
            (Value::AddressRange(a, al), Value::Word(b)) |
            (Value::Word(b), Value::AddressRange(a, al)) => {
                assert_eq!(al, 2);
                self.memory[a as usize + 0] == (b & 0xFF) as u8 && 
                self.memory[a as usize + 1] == ((b >> 8) & 0xFF) as u8
            }
            (Value::AddressRange(a, al), Value::DoubleWord(b)) |
            (Value::DoubleWord(b), Value::AddressRange(a, al)) => {
                assert_eq!(al, 4);
                self.memory[a as usize + 0] == (b & 0xFF) as u8 && 
                self.memory[a as usize + 1] == ((b >> 8) & 0xFF) as u8 &&
                self.memory[a as usize + 2] == ((b >> 16) & 0xFF) as u8 &&
                self.memory[a as usize + 3] == ((b >> 24) & 0xFF) as u8
            }
            (Value::FromAddress(a), Value::Byte(b)) |
            (Value::Byte(b), Value::FromAddress(a)) => {
                self.memory[a as usize] == b
            }
            (Value::FromAddress(a), Value::Word(b)) |
            (Value::Word(b), Value::FromAddress(a)) => {
                self.memory[a as usize + 0] == (b & 0xFF) as u8 && 
                self.memory[a as usize + 1] == ((b >> 8) & 0xFF) as u8
            }
            (Value::FromAddress(a), Value::DoubleWord(b)) |
            (Value::DoubleWord(b), Value::FromAddress(a)) => {
                self.memory[a as usize + 0] == (b & 0xFF) as u8 && 
                self.memory[a as usize + 1] == ((b >> 8) & 0xFF) as u8 &&
                self.memory[a as usize + 2] == ((b >> 16) & 0xFF) as u8 &&
                self.memory[a as usize + 3] == ((b >> 24) & 0xFF) as u8
            }
            (Value::Byte(a), Value::Byte(b)) => a == b,
            (Value::Word(a), Value::Word(b)) => a == b,
            (Value::DoubleWord(a), Value::DoubleWord(b)) => a == b,
            _ => panic!("invalid cmp"),
        }
    }

    fn write_value(&mut self, to: u32, len: Option<u32>, value: Value) {
        match value {
            Value::Byte(b) => {
                if len.is_some() {
                    assert_eq!(len, Some(1));
                }
                self.memory[to as usize] = b;
            }
            Value::Word(w) => {
                if len.is_some() {
                    assert_eq!(len, Some(2));
                }
                self.memory[to as usize] = (w & 0xFF) as u8;
                self.memory[(to + 1) as usize] = ((w >> 8) & 0xFF) as u8;
            }
            Value::DoubleWord(dw) => {
                if len.is_some() {
                    assert_eq!(len, Some(4));
                }
                self.memory[to as usize] = (dw & 0xFF) as u8;
                self.memory[(to + 1) as usize] = ((dw >> 8) & 0xFF) as u8;
                self.memory[(to + 2) as usize] = ((dw >> 16) & 0xFF) as u8;
                self.memory[(to + 3) as usize] = ((dw >> 24) & 0xFF) as u8;
            }
            Value::FromAddress(a) => {
                let len = len.unwrap();
                self.mem_copy(a, to, len);
            }
            Value::AddressRange(a, l) => {
                if len.is_some() {
                    assert_eq!(len, Some(l));
                }
                self.mem_copy(a, to, l);
            }
        }
    }

    fn run_op(&mut self, i: &ir::Instruction) -> Result<(), Error> {
        match *i {
            ir::Instruction::Assign(reg, ref val) |
            ir::Instruction::CastAssign(reg, ref val) => {
                let (to, len) = self.register_address(reg);
                let val = self.read_value(val);
                self.write_value(to, Some(len), val);
                Ok(())
            }
            ir::Instruction::BinaryOp(dest, op, ref a, ref b) => {
                let (to, len) = self.register_address(dest);
                match op {
                    ir::BinaryOp::IntOp(op, sign, ir::Size::Bit8) => {
                        let a = self.load_8bit(a);
                        let b = self.load_8bit(b);
                        let res = int_op_8(op, sign, a, b)?;
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                    ir::BinaryOp::IntOp(op, sign, ir::Size::Bit16) => {
                        let a = self.load_16bit(a);
                        let b = self.load_16bit(b);
                        let res = int_op_16(op, sign, a, b)?;
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                    ir::BinaryOp::IntOp(op, sign, ir::Size::Bit32) => {
                        let a = self.load_32bit(a);
                        let b = self.load_32bit(b);
                        let res = int_op_32(op, sign, a, b)?;
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                    ir::BinaryOp::Eq => {
                        assert_eq!(len, 1);
                        let a = self.read_value(a);
                        let b = self.read_value(b);
                        let res = self.compare_values(a, b) as u8;
                        self.memory[to as usize] = res;
                        Ok(())
                    }
                    ir::BinaryOp::Neq => {
                        assert_eq!(len, 1);
                        let a = self.read_value(a);
                        let b = self.read_value(b);
                        let res = !self.compare_values(a, b) as u8;
                        self.memory[to as usize] = res;
                        Ok(())
                    }
                    ir::BinaryOp::BitOp(op, ir::Size::Bit8) => {
                        let a = self.load_8bit(a);
                        let b = self.load_8bit(b);
                        let res = bit_op_8(op, a, b);
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                    ir::BinaryOp::BitOp(op, ir::Size::Bit16) => {
                        let a = self.load_16bit(a);
                        let b = self.load_16bit(b);
                        let res = bit_op_16(op, a, b);
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                    ir::BinaryOp::BitOp(op, ir::Size::Bit32) => {
                        let a = self.load_32bit(a);
                        let b = self.load_32bit(b);
                        let res = bit_op_32(op, a, b);
                        self.write_value(to, Some(len), res);
                        Ok(())
                    }
                }
            }
            ir::Instruction::Call(dest, ref sym, ref params) => {
                if "@plank_putc" == &*sym.0 {
                    // TODO: this should be proc
                    assert_eq!(params.len(), 1);
                    let val = self.load_8bit(&params[0]);
                    self.output.write_all(&[val])?;
                    return Ok(());
                } else if "@plank_getc" == &*sym.0 {
                    assert_eq!(params.len(), 0);
                    let mut buf = [0];
                    let result = match self.input.read(&mut buf)? {
                        0 => !0u32,
                        1 => buf[0] as u32,
                        _ => panic!("wut"),
                    };
                    let (ret, _) = self.register_address(dest);
                    self.write_value(ret, Some(4), Value::DoubleWord(result));
                    return Ok(());
                }
                let f = &self.program.functions[sym];
                let (ret, _) = self.register_address(dest);
                let stack_start = self.memory.len();
                let registers = self.allocate_registers(&f.registers);
                assert_eq!(f.parameters.len(), params.len());
                for (param, val) in f.parameters.iter().zip(params.iter()) {
                    let at = registers[param];
                    let len = f.registers[param].size;
                    let val = self.read_value(val);
                    self.write_value(at, Some(len), val);
                }
                let frame = StackFrame {
                    stack_start,
                    function: f,
                    registers,
                    current_block: f.start_block.ok_or(Error::MissingSymbol((*sym.0).into()))?,
                    current_op: 0,
                    return_address: Some(ret),
                };
                self.frames.push(::std::mem::replace(&mut self.current_frame, frame));
                Ok(())
            }
            ir::Instruction::CallProc(ref sym, ref params) => {
                let f = &self.program.functions[sym];
                let stack_start = self.memory.len();
                let registers = self.allocate_registers(&f.registers);
                assert_eq!(f.parameters.len(), params.len());
                for (param, val) in f.parameters.iter().zip(params.iter()) {
                    let at = registers[param];
                    let len = f.registers[param].size;
                    let val = self.read_value(val);
                    self.write_value(at, Some(len), val);
                }
                let frame = StackFrame {
                    stack_start,
                    function: f,
                    registers,
                    current_block: f.start_block.ok_or(Error::MissingSymbol((*sym.0).into()))?,
                    current_op: 0,
                    return_address: None,
                };
                self.frames.push(::std::mem::replace(&mut self.current_frame, frame));
                Ok(())
            }
            ir::Instruction::CallVirt(_, _, _) |
            ir::Instruction::CallProcVirt(_, _) => {
                unimplemented!("virtual calls")
            }
            ir::Instruction::DerefLoad(dest, ref address, offset) => {
                let address = self.load_32bit(address) + offset;
                let (to, len) = self.register_address(dest);
                self.write_value(to, Some(len), Value::FromAddress(address));
                Ok(())
            }
            ir::Instruction::DerefStore(ref address, offset, ref value) => {
                let address = self.load_32bit(address) + offset;
                let value = self.read_value(value);
                self.write_value(address, None, value);
                Ok(())
            }
            ir::Instruction::Drop(_) => Ok(()),
            ir::Instruction::Load(dest, reg, offset) => {
                let (to, len) = self.register_address(dest);
                let (from, _) = self.register_address(reg);
                self.mem_copy(to, from + offset, len);
                Ok(())
            }
            ir::Instruction::Store(dest, offset, ref value) => {
                let (to, _) = self.register_address(dest);
                let value = self.read_value(value);
                self.write_value(to + offset, None, value);
                Ok(())
            }
            ir::Instruction::TakeAddress(dest, reg, offset) => {
                let (to, _) = self.register_address(dest);
                let reg_at = self.current_frame.registers[&reg];
                let value = Value::DoubleWord(reg_at + offset);
                self.write_value(to, None, value);
                Ok(())
            }
            ir::Instruction::UnaryOp(reg, ir::UnaryOp::Negate(_, size), ref value) => {
                let (to, len) = self.register_address(reg);
                match size {
                    ir::Size::Bit8 => {
                        let val = self.load_8bit(value);
                        let res = (!val).wrapping_add(1);
                        self.write_value(to, Some(len), Value::Byte(res));
                    }
                    ir::Size::Bit16 => {
                        let val = self.load_16bit(value);
                        let res = (!val).wrapping_add(1);
                        self.write_value(to, Some(len), Value::Word(res));
                    }
                    ir::Size::Bit32 => {
                        let val = self.load_32bit(value);
                        let res = (!val).wrapping_add(1);
                        self.write_value(to, Some(len), Value::DoubleWord(res));
                    }
                }
                unimplemented!()
            }
        }
    }

    fn run(&mut self) -> Result<(), Error> {
        loop {
            let block = self.current_block();
            if self.current_frame.current_op == block.ops.len() {
                match block.end {
                    ir::BlockEnd::Jump(block) => {
                        self.current_frame.current_block = block;
                        self.current_frame.current_op = 0;
                    }
                    ir::BlockEnd::Branch(ref val, a, b) => {
                        self.current_frame.current_op = 0;
                        if self.load_8bit(val) != 0 {
                            self.current_frame.current_block = a;
                        } else {
                            self.current_frame.current_block = b;
                        }
                    }
                    ir::BlockEnd::Return(ref val) => {
                        let val = self.read_value(val);
                        let len = self.current_frame.function.output_layout.unwrap().size;
                        let to = self.current_frame.return_address.unwrap();
                        self.write_value(to, Some(len), val);
                        match self.frames.pop() {
                            Some(frame) => self.current_frame = frame,
                            None => return Ok(()),
                        }
                    }
                    ir::BlockEnd::ReturnProc => {
                        assert!(self.current_frame.return_address.is_none());
                        self.memory.truncate(self.current_frame.stack_start);
                        match self.frames.pop() {
                            Some(frame) => self.current_frame = frame,
                            None => return Ok(()),
                        }
                    }
                }
            } else {
                let op = &block.ops[self.current_frame.current_op];
                self.current_frame.current_op += 1;
                self.run_op(op)?;
            }
        }
    }
}

fn int_op_32(op: ir::IntOp, sign: ir::Signedness, a: u32, b: u32) -> Result<Value, Error> {
    match (op, sign) {
        (ir::IntOp::Add, _) => Ok(Value::DoubleWord(a.wrapping_add(b))),
        (ir::IntOp::Sub, _) => Ok(Value::DoubleWord(a.wrapping_sub(b))),
        (ir::IntOp::Greater, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a > b) as u8))
        }
        (ir::IntOp::Greater, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i32 > b as i32) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a >= b) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i32 >= b as i32) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a < b) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Signed) => {
            Ok(Value::Byte(((a as i32) < b as i32) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a <= b) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i32 <= b as i32) as u8))
        }
        (ir::IntOp::Mul, ir::Signedness::Unsigned) => {
            Ok(Value::DoubleWord(a.wrapping_mul(b)))
        }
        (ir::IntOp::Mul, ir::Signedness::Signed) => {
            Ok(Value::DoubleWord((a as i32).wrapping_mul(b as i32) as u32))
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) => {
            Ok(Value::DoubleWord(a / b))
        }
        (ir::IntOp::Div, ir::Signedness::Signed) if b as i32 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Signed) => {
            Ok(Value::DoubleWord((a as i32).wrapping_div(b as i32) as u32))
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) => {
            Ok(Value::DoubleWord(a % b))
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) if b as i32 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) => {
            Ok(Value::DoubleWord((a as i32).wrapping_rem(b as i32) as u32))
        }
    }
}

fn int_op_16(op: ir::IntOp, sign: ir::Signedness, a: u16, b: u16) -> Result<Value, Error> {
    match (op, sign) {
        (ir::IntOp::Add, _) => Ok(Value::Word(a.wrapping_add(b))),
        (ir::IntOp::Sub, _) => Ok(Value::Word(a.wrapping_sub(b))),
        (ir::IntOp::Greater, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a > b) as u8))
        }
        (ir::IntOp::Greater, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i16 > b as i16) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a >= b) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i16 >= b as i16) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a < b) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Signed) => {
            Ok(Value::Byte(((a as i16) < b as i16) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a <= b) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i16 <= b as i16) as u8))
        }
        (ir::IntOp::Mul, ir::Signedness::Unsigned) => {
            Ok(Value::Word(a.wrapping_mul(b)))
        }
        (ir::IntOp::Mul, ir::Signedness::Signed) => {
            Ok(Value::Word((a as i16).wrapping_mul(b as i16) as u16))
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) => {
            Ok(Value::Word(a / b))
        }
        (ir::IntOp::Div, ir::Signedness::Signed) if b as i16 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Signed) => {
            Ok(Value::Word((a as i16).wrapping_div(b as i16) as u16))
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) => {
            Ok(Value::Word(a % b))
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) if b as i16 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) => {
            Ok(Value::Word((a as i16).wrapping_rem(b as i16) as u16))
        }
    }
}

fn int_op_8(op: ir::IntOp, sign: ir::Signedness, a: u8, b: u8) -> Result<Value, Error> {
    match (op, sign) {
        (ir::IntOp::Add, _) => Ok(Value::Byte(a.wrapping_add(b))),
        (ir::IntOp::Sub, _) => Ok(Value::Byte(a.wrapping_sub(b))),
        (ir::IntOp::Greater, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a > b) as u8))
        }
        (ir::IntOp::Greater, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8 > b as i8) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a >= b) as u8))
        }
        (ir::IntOp::GreaterEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8 >= b as i8) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a < b) as u8))
        }
        (ir::IntOp::Less, ir::Signedness::Signed) => {
            Ok(Value::Byte(((a as i8) < b as i8) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Unsigned) => {
            Ok(Value::Byte((a <= b) as u8))
        }
        (ir::IntOp::LessEq, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8 <= b as i8) as u8))
        }
        (ir::IntOp::Mul, ir::Signedness::Unsigned) => {
            Ok(Value::Byte(a.wrapping_mul(b)))
        }
        (ir::IntOp::Mul, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8).wrapping_mul(b as i8) as u8))
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Unsigned) => {
            Ok(Value::Byte(a / b))
        }
        (ir::IntOp::Div, ir::Signedness::Signed) if b as i8 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Div, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8).wrapping_div(b as i8) as u8))
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) if b == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Unsigned) => {
            Ok(Value::Byte(a % b))
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) if b as i8 == 0 => {
            Err(Error::DivisionByZero)
        }
        (ir::IntOp::Mod, ir::Signedness::Signed) => {
            Ok(Value::Byte((a as i8).wrapping_rem(b as i8) as u8))
        }
    }
}

fn bit_op_32(op: ir::BitOp, a: u32, b: u32) -> Value {
    match op {
        ir::BitOp::And => Value::DoubleWord(a & b),
        ir::BitOp::Or => Value::DoubleWord(a | b),
        ir::BitOp::Xor => Value::DoubleWord(a ^ b),
    }
}

fn bit_op_16(op: ir::BitOp, a: u16, b: u16) -> Value {
    match op {
        ir::BitOp::And => Value::Word(a & b),
        ir::BitOp::Or => Value::Word(a | b),
        ir::BitOp::Xor => Value::Word(a ^ b),
    }
}

fn bit_op_8(op: ir::BitOp, a: u8, b: u8) -> Value {
    match op {
        ir::BitOp::And => Value::Byte(a & b),
        ir::BitOp::Or => Value::Byte(a | b),
        ir::BitOp::Xor => Value::Byte(a ^ b),
    }
}
