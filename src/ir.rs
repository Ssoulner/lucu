use std::{
    collections::HashMap,
    fmt::{self, Display},
    write,
};

use crate::{
    analyzer::{Analysis, Definition, EffFunIdx, Val, DEBUG, INT, STR},
    parser::{self, ExprIdx, Expression, Op, ParseContext, ReturnType, AST},
    vecmap::VecMap,
};

#[derive(Debug)]
pub struct Procedure {
    pub inputs: Vec<Reg>,

    // whether or not this is an effect handler that accepts a continuation parameter
    pub is_handler: bool,

    pub output: Type,
    pub blocks: VecMap<BlockIdx, Block>,
    pub start: BlockIdx,

    pub debug_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Pointer,
    Frame,
    Aggregate(TypeIdx),

    Never,
    None,
}

impl Type {
    fn from_type(asys: &Analysis, typ: &parser::Type) -> Type {
        match asys.values[typ.ident] {
            STR => Type::Aggregate(SLICE),
            INT => Type::Int,
            _ => panic!("unknown type"),
        }
    }
    fn from_return(asys: &Analysis, typ: Option<&parser::ReturnType>) -> Type {
        match typ {
            Some(ReturnType::Type(t)) => Type::from_type(asys, t),
            Some(ReturnType::Never) => Type::Never,
            None => Type::None,
        }
    }
    fn outputs_value(&self) -> bool {
        !matches!(self, Type::None)
    }
}

#[derive(Default)]
pub struct AggregateType {
    pub children: Vec<Type>,
}

impl Procedure {
    fn instructions(&self) -> impl Iterator<Item = &Instruction> {
        self.blocks.values().flat_map(|b| b.instructions.iter())
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct BlockIdx(usize);

impl From<BlockIdx> for usize {
    fn from(value: BlockIdx) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct TypeIdx(usize);

impl From<TypeIdx> for usize {
    fn from(value: TypeIdx) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct Reg(usize);

impl Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "R{:02}", self.0)
    }
}

impl Display for BlockIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}", self.0)
    }
}

impl From<Reg> for usize {
    fn from(value: Reg) -> Self {
        value.0
    }
}

#[derive(Debug, Default)]
pub struct Block {
    pub instructions: Vec<Instruction>,
    pub next: Option<BlockIdx>,
}

#[derive(Debug)]
pub enum Instruction {
    Init(Reg, u64),
    InitString(Reg, String),
    Copy(Reg, Reg),

    // conditionals
    JmpNZ(Reg, BlockIdx),
    Phi(Reg, [(Reg, BlockIdx); 2]),

    // operations (r0 = r1 op r2)
    Equals(Reg, Reg, Reg),
    Div(Reg, Reg, Reg),
    Mul(Reg, Reg, Reg),
    Add(Reg, Reg, Reg),
    Sub(Reg, Reg, Reg),

    // call procedure, put return into reg, call with arguments
    Reset(ProcIdx, Option<Reg>, Vec<Reg>, Reg, Vec<Reg>), // puts handler closure into reg and calls
    Shift(ProcIdx, Option<Reg>, Vec<Reg>, Reg), // shift into procedure with frame parameter
    Call(ProcIdx, Option<Reg>, Vec<Reg>),       // regular call

    // return statements for effect handlers
    Resume(Option<Reg>, Option<Reg>), // resume the continuation with optional value

    // return statement for regular procedures
    Return(Option<Reg>), // return with optional value

    // print
    PrintNum(Reg), // print number in register
    PrintStr(Reg), // dereference register and print string

    // create aggregate type
    Aggregate(Reg, Vec<Reg>),
    Member(Reg, Reg, usize),
}

impl Instruction {
    fn calls_proc(&self) -> Option<ProcIdx> {
        match *self {
            Instruction::Reset(proc, _, _, _, _)
            | Instruction::Shift(proc, _, _, _)
            | Instruction::Call(proc, _, _) => Some(proc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProcIdx(usize);

impl From<ProcIdx> for usize {
    fn from(value: ProcIdx) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct HandlerIdx(usize);

impl From<HandlerIdx> for usize {
    fn from(value: HandlerIdx) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ProcIdent {
    fun: Val,
    handlers: Box<[HandlerIdx]>,
}

#[derive(Debug)]
struct Handler {
    effect: Val,
    closure: TypeIdx,
    procs: VecMap<EffFunIdx, ProcIdx>,
}

#[derive(Default)]
struct Scope<'a> {
    parent: Option<&'a Scope<'a>>,
    regs: HashMap<Val, Reg>,
    captures: HashMap<Val, Reg>,
}

impl<'a> Scope<'a> {
    fn get_parent(&self, key: Val) -> Option<Reg> {
        match self.regs.get(&key) {
            Some(&v) => Some(v),
            None => self.parent.map(|p| p.get_parent(key)).flatten(),
        }
    }
    fn get_or_capture(&mut self, ir: &mut IRContext, key: Val) -> Option<Reg> {
        match self.regs.get(&key) {
            Some(&v) => Some(v),
            None => match self.parent.map(|p| p.get_parent(key)).flatten() {
                Some(reg) => {
                    if !self.captures.contains_key(&key) {
                        let capture = ir.copy_reg(reg);
                        self.captures.insert(key, capture);
                        Some(capture)
                    } else {
                        Some(self.captures[&key])
                    }
                }
                None => None,
            },
        }
    }
    fn child(&self) -> Scope {
        Scope {
            parent: Some(self),
            regs: HashMap::new(),
            captures: HashMap::new(),
        }
    }
}

struct IRContext<'a> {
    ir: IR,

    proc_map: HashMap<ProcIdent, ProcIdx>,
    handlers: VecMap<HandlerIdx, Handler>,

    ast: &'a AST,
    ctx: &'a ParseContext,
    asys: &'a Analysis,
}

impl<'a> IRContext<'a> {
    fn copy_reg(&mut self, reg: Reg) -> Reg {
        let typ = self.ir.types[reg];
        self.next_reg(typ)
    }
    fn next_reg(&mut self, typ: Type) -> Reg {
        self.ir.types.push(Reg, typ)
    }
}

pub struct IR {
    pub procs: VecMap<ProcIdx, Procedure>,
    pub main: ProcIdx,

    pub types: VecMap<Reg, Type>,
    pub aggregates: VecMap<TypeIdx, AggregateType>,
}

impl Display for VecMap<ProcIdx, Procedure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for proc in self.values() {
            // write proc signature
            write!(f, "{}", proc.debug_name)?;

            if proc.inputs.len() > 0 {
                write!(f, " ( ")?;
                for &r in proc.inputs.iter() {
                    if r != *proc.inputs.first().unwrap() {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", r)?;
                }
                write!(f, " )")?;
            }

            if proc.is_handler {
                write!(f, " < cont >")?;
            }

            write!(f, " {{\n")?;

            // write blocks
            for (i, block) in proc.blocks.values().enumerate() {
                // write label
                if i > 0 {
                    writeln!(f, "{}:", BlockIdx(i))?;
                }

                // write instructions
                for instr in block.instructions.iter() {
                    write!(f, "  ")?;
                    match *instr {
                        Instruction::Init(r, v) => writeln!(f, "{} <- {}", r, v)?,
                        Instruction::InitString(r, ref v) => writeln!(f, "{} <- \"{}\"", r, v)?,
                        Instruction::Copy(r, v) => writeln!(f, "{} <- {}", r, v)?,
                        Instruction::JmpNZ(r, b) => writeln!(f, "       jnz {}, {}", r, b)?,
                        Instruction::Phi(r, [(r1, b1), (r2, b2)]) => {
                            writeln!(f, "{} <- phi [ {}, {} ], [ {}, {} ]", r, r1, b1, r2, b2,)?
                        }
                        Instruction::Equals(out, left, right) => {
                            writeln!(f, "{} <- {} == {}", out, left, right)?
                        }
                        Instruction::Div(out, left, right) => {
                            writeln!(f, "{} <- {} / {}", out, left, right)?
                        }
                        Instruction::Mul(out, left, right) => {
                            writeln!(f, "{} <- {} * {}", out, left, right)?
                        }
                        Instruction::Add(out, left, right) => {
                            writeln!(f, "{} <- {} + {}", out, left, right)?
                        }
                        Instruction::Sub(out, left, right) => {
                            writeln!(f, "{} <- {} - {}", out, left, right)?
                        }
                        Instruction::Reset(proc, out, ref args, closure, ref closure_args) => {
                            writeln!(
                                f,
                                "{}rst {}, {{ {} }}, {}{}",
                                match out {
                                    Some(out) => format!("{} <- ", out),
                                    None => "       ".into(),
                                },
                                closure,
                                closure_args
                                    .iter()
                                    .map(|r| r.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                self[proc].debug_name,
                                match args
                                    .iter()
                                    .map(|r| r.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                                    .as_str()
                                {
                                    "" => "".into(),
                                    a => format!(" ( {} )", a),
                                },
                            )?
                        }
                        Instruction::Shift(proc, out, ref args, frame) => writeln!(
                            f,
                            "{}sft {}{} < {} >",
                            match out {
                                Some(out) => format!("{} <- ", out),
                                None => "       ".into(),
                            },
                            self[proc].debug_name,
                            match args
                                .iter()
                                .map(|r| r.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                                .as_str()
                            {
                                "" => "".into(),
                                a => format!(" ( {} )", a),
                            },
                            frame,
                        )?,
                        Instruction::Call(proc, out, ref args) => writeln!(
                            f,
                            "{}cal {}{}",
                            match out {
                                Some(out) => format!("{} <- ", out),
                                None => "       ".into(),
                            },
                            self[proc].debug_name,
                            match args
                                .iter()
                                .map(|r| r.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                                .as_str()
                            {
                                "" => "".into(),
                                a => format!(" ( {} )", a),
                            },
                        )?,
                        Instruction::Resume(out, r) => writeln!(
                            f,
                            "{}res{}",
                            match out {
                                Some(out) => format!("{} <- ", out),
                                None => "       ".into(),
                            },
                            match r {
                                Some(r) => format!(" {}", r),
                                None => "".into(),
                            }
                        )?,
                        Instruction::Return(r) => writeln!(
                            f,
                            "       ret{}",
                            match r {
                                Some(r) => format!(" {}", r),
                                None => "".into(),
                            }
                        )?,
                        Instruction::PrintNum(r) => writeln!(f, "       putint {}", r)?,
                        Instruction::PrintStr(r) => writeln!(f, "       putstr {}", r)?,
                        Instruction::Aggregate(r, ref v) => writeln!(
                            f,
                            "{} <- {{ {} }}",
                            r,
                            v.iter()
                                .map(|r| r.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )?,
                        Instruction::Member(r, a, m) => writeln!(f, "{} <- {}.{}", r, a, m)?,
                    }
                }

                // write next
                if let Some(b) = block.next {
                    if usize::from(b) != i + 1 {
                        writeln!(f, "         jmp L{}", usize::from(b))?;
                    }
                }
            }

            // end proc
            writeln!(f, "}}")?;
        }
        Ok(())
    }
}

const SLICE: TypeIdx = TypeIdx(0);

pub fn generate_ir(ast: &AST, ctx: &ParseContext, asys: &Analysis) -> IR {
    let mut ir = IRContext {
        proc_map: HashMap::new(),
        handlers: VecMap::new(),
        ast,
        ctx,
        asys,
        ir: IR {
            procs: VecMap::new(),
            main: ProcIdx(0),
            types: VecMap::new(),
            aggregates: VecMap::new(),
        },
    };

    ir.ir.aggregates.push_value(AggregateType {
        children: vec![Type::Pointer, Type::Int],
    });

    // define putint
    let debug_closure = ir.ir.aggregates.push(TypeIdx, AggregateType::default());

    let inputs = vec![
        ir.next_reg(Type::Int),
        ir.next_reg(Type::Aggregate(debug_closure)),
    ];

    let putint = ir.ir.procs.push(
        ProcIdx,
        Procedure {
            inputs,

            is_handler: false,
            output: Type::None,

            blocks: vec![Block {
                instructions: vec![Instruction::PrintNum(Reg(0)), Instruction::Return(None)],
                next: None,
            }]
            .into(),
            start: BlockIdx(0),
            debug_name: "putint".into(),
        },
    );

    // define debug
    let debug = ir.handlers.push(
        HandlerIdx,
        Handler {
            effect: DEBUG,
            procs: vec![putint].into(),
            closure: debug_closure,
        },
    );

    // generate
    // TODO: main not found
    let main = asys.main.expect("no main function");
    let fun = &ir.ast.functions[main];
    let val = ir.asys.values[fun.decl.name];

    let params: Box<[(Val, Type)]> = fun
        .decl
        .sign
        .inputs
        .values()
        .map(|&(ident, ref typ)| (ir.asys.values[ident], Type::from_type(&ir.asys, typ)))
        .collect();

    let output = Type::from_return(&ir.asys, fun.decl.sign.output.as_ref());

    let debug_reg = ir.next_reg(Type::Aggregate(debug_closure));
    let mut scope = Scope::default();
    scope.regs.insert(DEBUG, debug_reg);

    ir.ir.main = generate_func(
        &mut ir,
        Some(val),
        &[debug],
        &params,
        fun.body,
        output,
        false,
        "main".into(),
        &mut scope,
    );

    ir.ir.procs[ir.ir.main].blocks[BlockIdx(0)]
        .instructions
        .insert(0, Instruction::Aggregate(debug_reg, Vec::new()));

    ir.ir
}

fn generate_func(
    ir: &mut IRContext,
    fun: Option<Val>,
    handlers: &[HandlerIdx],
    params: &[(Val, Type)],
    body: ExprIdx,
    output: Type,
    is_handler: bool,
    debug_name: String,
    scope: &mut Scope,
) -> ProcIdx {
    // add new proc to list
    let mut blocks = VecMap::new();
    let start = blocks.push(BlockIdx, Block::default());

    let proc_idx = ir.ir.procs.push(
        ProcIdx,
        Procedure {
            inputs: Vec::new(),
            is_handler,
            output,
            start,
            debug_name: debug_name.clone(),

            // these will be defined at the end
            blocks: VecMap::new(),
        },
    );

    if let Some(fun) = fun {
        let ident = ProcIdent {
            fun,
            handlers: handlers.into(),
        };
        ir.proc_map.insert(ident, proc_idx);
    }

    // add params to vars
    for &(val, typ) in params {
        let reg = ir.next_reg(typ);
        scope.regs.insert(val, reg);
        ir.ir.procs[proc_idx].inputs.push(reg);
    }

    // generate code
    let mut end = start;
    let ret = generate_expr(
        ir,
        handlers,
        body,
        &mut blocks,
        &mut end,
        is_handler,
        &debug_name,
        scope,
    )
    .filter(|_| output.outputs_value());

    if !matches!(
        blocks[end].instructions.last(),
        Some(Instruction::Return(_))
    ) {
        // add return if we haven't already
        let ret = if is_handler {
            // tail call resume for handlers
            let res = ret;
            let ret = res.map(|res| ir.copy_reg(res));
            blocks[end].instructions.push(Instruction::Resume(ret, res));
            ret
        } else {
            ret
        };
        blocks[end].instructions.push(Instruction::Return(ret));
    }

    // get captures from last argument
    if scope.captures.len() > 0 {
        let mut captures = scope.captures.values().copied().collect::<Vec<_>>();
        captures.sort_unstable_by_key(|reg| reg.0);

        let closure = ir.ir.procs[proc_idx].inputs.last().unwrap().clone();
        for (i, &capture) in captures.iter().enumerate() {
            blocks[start]
                .instructions
                .insert(0, Instruction::Member(capture, closure, i + 1));
        }
    }

    // return proc
    ir.ir.procs[proc_idx].blocks = blocks;
    proc_idx
}

fn generate_reset(
    ir: &mut IRContext,
    handlers: &[HandlerIdx],
    body: ExprIdx,
    debug_name: String,
    scope: &mut Scope,
) -> ProcIdx {
    // create blocks
    let mut blocks = VecMap::new();
    let start = blocks.push(BlockIdx, Block::default());

    // generate code
    let mut end = start;
    let ret = generate_expr(
        ir,
        handlers,
        body,
        &mut blocks,
        &mut end,
        false,
        &debug_name,
        scope,
    );

    if !matches!(
        blocks[end].instructions.last(),
        Some(Instruction::Return(_))
    ) {
        // add return if we haven't already
        blocks[end].instructions.push(Instruction::Return(ret));
    }

    let output = ret.map(|ret| ir.ir.types[ret]).unwrap_or(Type::None);

    // return proc
    ir.ir.procs.push(
        ProcIdx,
        Procedure {
            inputs: scope.captures.values().copied().collect(),
            is_handler: false,
            output,
            start,
            debug_name,
            blocks,
        },
    )
}

fn generate_expr(
    ir: &mut IRContext,
    handlers: &[HandlerIdx],
    expr: ExprIdx,
    blocks: &mut VecMap<BlockIdx, Block>,
    block: &mut BlockIdx,
    is_handler: bool,
    debug_name: &str,
    scope: &mut Scope,
) -> Option<Reg> {
    use Expression as E;
    match ir.ctx.exprs[expr].0 {
        E::Body(ref body) => {
            for &expr in body.main.iter() {
                generate_expr(
                    ir, handlers, expr, blocks, block, is_handler, debug_name, scope,
                );
            }
            body.last.and_then(|expr| {
                generate_expr(
                    ir, handlers, expr, blocks, block, is_handler, debug_name, scope,
                )
            })
        }
        E::Call(func, ref args) => {
            // TODO: currently we assume func is an Ident expr
            match ir.ctx.exprs[func].0 {
                E::Ident(id) => {
                    let val = ir.asys.values[id];

                    // get base registers
                    let mut reg_args = Vec::new();
                    for &expr in args {
                        let reg = generate_expr(
                            ir, handlers, expr, blocks, block, is_handler, debug_name, scope,
                        )
                        .expect("function call argument does not return a value");
                        reg_args.push(reg);
                    }

                    // check handlers
                    match ir.asys.defs[val] {
                        Definition::EffectFunction(eff_val, eff_fun_idx) => {
                            // get handler
                            let handler = handlers
                                .iter()
                                .map(|&idx| &ir.handlers[idx])
                                .find(|handler| handler.effect == eff_val)
                                .expect("handler of effect function is not in scope");

                            let proc_idx = handler.procs[eff_fun_idx];

                            let typ = ir.ir.procs[proc_idx].output;
                            let output = Some(ir.next_reg(typ));

                            // get closure
                            let closure = scope
                                .get_or_capture(ir, eff_val)
                                .expect("handler closure is not in scope");
                            reg_args.push(closure);

                            // execute handler
                            let proc = &ir.ir.procs[proc_idx];
                            if proc.is_handler {
                                // get frame parameter
                                let frame_param = ir.next_reg(Type::Frame);
                                blocks[*block].instructions.push(Instruction::Member(
                                    frame_param,
                                    closure,
                                    0,
                                ));

                                // shift to handler
                                blocks[*block].instructions.push(Instruction::Shift(
                                    proc_idx,
                                    output,
                                    reg_args,
                                    frame_param,
                                ));
                            } else {
                                // call handler as function
                                blocks[*block]
                                    .instructions
                                    .push(Instruction::Call(proc_idx, output, reg_args));
                            }

                            output
                        }
                        Definition::Function(func_idx) => {
                            // create proc identity
                            let fun = &ir.ast.functions[func_idx];

                            let effects: Box<[HandlerIdx]> = fun
                                .decl
                                .sign
                                .effects
                                .iter()
                                .map(|&e| {
                                    let effect = ir.asys.values[e];
                                    handlers
                                        .iter()
                                        .find(|&&h| ir.handlers[h].effect == effect)
                                        .unwrap()
                                        .clone()
                                })
                                .collect();

                            let procident = ProcIdent {
                                fun: val,
                                handlers: effects,
                            };

                            reg_args.extend(procident.handlers.iter().map(|&idx| {
                                scope
                                    .get_or_capture(ir, ir.handlers[idx].effect)
                                    .expect("handler closure not in scope")
                            }));

                            // get proc
                            let proc_idx = if !ir.proc_map.contains_key(&procident) {
                                let handlers = &procident.handlers;

                                // get params
                                let mut params: Vec<(Val, Type)> = fun
                                    .decl
                                    .sign
                                    .inputs
                                    .values()
                                    .map(|&(ident, ref typ)| {
                                        (ir.asys.values[ident], Type::from_type(&ir.asys, typ))
                                    })
                                    .collect();

                                params.extend(procident.handlers.iter().map(|&idx| {
                                    (
                                        ir.handlers[idx].effect,
                                        Type::Aggregate(ir.handlers[idx].closure),
                                    )
                                }));

                                let output =
                                    Type::from_return(&ir.asys, fun.decl.sign.output.as_ref());

                                // generate debug name
                                let mut debug_name = ir.ctx.idents[fun.decl.name].0.clone();

                                if handlers.len() > 0 {
                                    debug_name += "/";

                                    for &handler in handlers.iter() {
                                        let eff_val = ir.handlers[handler].effect;
                                        let eff_name = ir
                                            .ast
                                            .effects
                                            .values()
                                            .find(|e| ir.asys.values[e.name] == eff_val)
                                            .map(|e| ir.ctx.idents[e.name].0.as_str())
                                            .unwrap_or("debug"); // TODO: support other builtin effects

                                        debug_name += eff_name;
                                        debug_name += "#";
                                        debug_name += usize::from(handler).to_string().as_str();
                                        debug_name += "_";
                                    }

                                    debug_name.pop();
                                }

                                // generate func
                                generate_func(
                                    ir,
                                    Some(val),
                                    handlers,
                                    &params,
                                    fun.body,
                                    output,
                                    false,
                                    debug_name,
                                    scope,
                                )
                            } else {
                                ir.proc_map[&procident]
                            };
                            let proc = &ir.ir.procs[proc_idx];

                            // execute proc
                            let output = Some(ir.next_reg(proc.output));
                            blocks[*block]
                                .instructions
                                .push(Instruction::Call(proc_idx, output, reg_args));
                            output
                        }

                        Definition::Parameter(_) => todo!(),
                        Definition::Effect(_) => todo!(),
                        Definition::Builtin => todo!(),
                    }
                }
                _ => todo!(),
            }
        }
        E::Member(_, _) => todo!(),
        E::IfElse(cond, yes, no) => {
            let cond = generate_expr(
                ir, handlers, cond, blocks, block, is_handler, debug_name, scope,
            )
            .expect("condition has no value");

            match no {
                Some(no) => {
                    let no_start = blocks.push(BlockIdx, Block::default());

                    let mut no_end = no_start;
                    let no_reg = generate_expr(
                        ir,
                        handlers,
                        no,
                        blocks,
                        &mut no_end,
                        is_handler,
                        debug_name,
                        scope,
                    );

                    let yes_start = blocks.push(BlockIdx, Block::default());

                    blocks[*block]
                        .instructions
                        .push(Instruction::JmpNZ(cond, yes_start));

                    let mut yes_end = yes_start;
                    let yes_reg = generate_expr(
                        ir,
                        handlers,
                        yes,
                        blocks,
                        &mut yes_end,
                        is_handler,
                        debug_name,
                        scope,
                    );

                    let end = blocks.push(BlockIdx, Block::default());

                    blocks[*block].next = Some(no_start);
                    blocks[yes_end].next = Some(end);
                    blocks[no_end].next = Some(end);
                    *block = end;

                    if let (Some(yes), Some(no)) = (yes_reg, no_reg) {
                        if ir.ir.types[yes] == ir.ir.types[no] {
                            let out = ir.copy_reg(yes);
                            blocks[*block]
                                .instructions
                                .push(Instruction::Phi(out, [(yes, yes_end), (no, no_end)]));
                            Some(out)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                None => {
                    let yes_start = blocks.push(BlockIdx, Block::default());

                    blocks[*block]
                        .instructions
                        .push(Instruction::JmpNZ(cond, yes_start));

                    let mut yes_end = yes_start;
                    generate_expr(
                        ir,
                        handlers,
                        yes,
                        blocks,
                        &mut yes_end,
                        is_handler,
                        debug_name,
                        scope,
                    );

                    let end = blocks.push(BlockIdx, Block::default());

                    blocks[*block].next = Some(end);
                    blocks[yes_end].next = Some(end);
                    *block = end;

                    None
                }
            }
        }
        E::Op(left, op, right) => {
            let left = generate_expr(
                ir, handlers, left, blocks, block, is_handler, debug_name, scope,
            )
            .expect("left operand has no value");

            let right = generate_expr(
                ir, handlers, right, blocks, block, is_handler, debug_name, scope,
            )
            .expect("right operand has no value");

            // TODO: ops with different return types
            let out = ir.next_reg(Type::Int);

            let instr = match op {
                Op::Equals => Instruction::Equals(out, left, right),
                Op::Divide => Instruction::Div(out, left, right),
                Op::Multiply => Instruction::Mul(out, left, right),
                Op::Subtract => Instruction::Sub(out, left, right),
                Op::Add => Instruction::Add(out, left, right),
            };
            blocks[*block].instructions.push(instr);

            Some(out)
        }
        E::Break(value) => {
            // get break value
            let reg = value.and_then(|expr| {
                generate_expr(
                    ir, handlers, expr, blocks, block, is_handler, debug_name, scope,
                )
            });

            // TODO: we assume this is top level inside a handler
            blocks[*block].instructions.push(Instruction::Return(reg));

            // break returns any type
            Some(ir.next_reg(Type::Never))
        }
        E::TryWith(body, handler) => {
            // get handler
            let ast_handler = match ir.ctx.exprs[handler].0 {
                E::Handler(ref handler) => handler,
                _ => todo!(),
            };

            // get effect
            let eff_ident = ast_handler.effect;
            let eff_val = ir.asys.values[eff_ident];
            let eff_idx = match ir.asys.defs[eff_val] {
                Definition::Effect(eff_idx) => eff_idx,
                _ => panic!("handler has non-effect as effect value"),
            };
            let effect = &ir.ast.effects[eff_idx];

            // generate handler
            let closure = ir.ir.aggregates.push(TypeIdx, AggregateType::default());
            let closure_reg = ir.next_reg(Type::Aggregate(closure));

            // put frame register in scope
            let frame_reg = ir.next_reg(Type::Frame);
            ir.ir.aggregates[closure].children.push(Type::Frame);
            scope.regs.insert(eff_val, closure_reg);

            // generate handler
            let handler_idx = ir.handlers.push(
                HandlerIdx,
                Handler {
                    closure,
                    effect: eff_val,
                    procs: VecMap::filled(effect.functions.len(), ProcIdx(usize::MAX)),
                },
            );

            let mut child = scope.child();
            for fun in ast_handler.functions.iter() {
                let val = ir.asys.values[fun.decl.name];
                let eff_fun_idx = match ir.asys.defs[val] {
                    Definition::EffectFunction(_, eff_fun_idx) => eff_fun_idx,
                    _ => panic!("handler has non-effect-function as a function value"),
                };

                // get params
                let mut params: Vec<(Val, Type)> = fun
                    .decl
                    .sign
                    .inputs
                    .values()
                    .map(|&(ident, ref typ)| {
                        (ir.asys.values[ident], Type::from_type(&ir.asys, typ))
                    })
                    .collect();
                params.push((val, Type::Aggregate(closure)));

                let output = Type::from_return(&ir.asys, fun.decl.sign.output.as_ref());

                // generate debug name
                let eff_name = ir.ctx.idents[eff_ident].0.as_str();
                let proc_name = ir.ctx.idents[fun.decl.name].0.as_str();
                let debug_name =
                    format!("{}#{}__{}", eff_name, usize::from(handler_idx), proc_name);
                // TODO: add handlers of proc

                // generate handler proc
                let proc_idx = generate_func(
                    ir, None, handlers, // TODO: add handlers of proc
                    &params, fun.body, output, true, debug_name, &mut child,
                );

                // add to handler
                ir.handlers[handler_idx].procs[eff_fun_idx] = proc_idx;
            }

            // create handler closure
            let mut captures = child
                .captures
                .iter()
                .map(|(&k, &v)| (k, v))
                .collect::<Vec<_>>();

            captures.sort_unstable_by_key(|&(_, reg)| reg.0);
            ir.ir.aggregates[closure]
                .children
                .extend(captures.iter().map(|&(_, reg)| ir.ir.types[reg]));

            let aggregate = captures
                .iter()
                .map(|&(val, _)| scope.get_or_capture(ir, val).expect("value not in scope"))
                .collect();

            // add handler to handler list
            let mut subhandlers = Vec::new();
            subhandlers.extend_from_slice(handlers);

            match subhandlers
                .iter()
                .position(|&idx| ir.handlers[idx].effect == eff_val)
            {
                Some(pos) => {
                    // replace existing handler
                    subhandlers[pos] = handler_idx;
                }
                None => {
                    // push new handler
                    subhandlers.push(handler_idx);
                }
            }

            // generate reset
            let debug_name = format!("{}__reset#{}", debug_name, usize::from(frame_reg));

            let mut child = scope.child();
            let proc_idx = generate_reset(ir, &subhandlers, body, debug_name, &mut child);

            let input_regs = child
                .captures
                .keys()
                .map(|&val| scope.get_or_capture(ir, val).expect("value not in scope"))
                .collect();

            let proc = &ir.ir.procs[proc_idx];

            // execute proc
            let output = Some(ir.next_reg(proc.output));

            blocks[*block].instructions.push(Instruction::Reset(
                proc_idx,
                output,
                input_regs,
                closure_reg,
                aggregate,
            ));

            output
        }
        E::Handler(_) => todo!(),
        E::String(ref s) => {
            let reg = ir.next_reg(Type::Aggregate(SLICE));

            blocks[*block]
                .instructions
                .push(Instruction::InitString(reg, s.clone()));

            Some(reg)
        }

        E::Int(i) => {
            let reg = ir.next_reg(Type::Int);

            // TODO: handle overflow
            blocks[*block]
                .instructions
                .push(Instruction::Init(reg, i as i64 as u64));

            Some(reg)
        }

        E::Ident(id) => {
            // TODO: globals
            let val = ir.asys.values[id];
            let reg = scope
                .get_or_capture(ir, val)
                .expect("value is not loaded in scope");
            Some(reg)
        }
        E::Error => todo!(),
    }
}
