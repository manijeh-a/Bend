#![feature(box_patterns)]
#![feature(let_chains)]

use diagnostics::{Info, Warning};
use hvmc::{
  ast::{book_to_runtime, show_book, Net},
  run::{Def, Rewrites},
};
use hvmc_net::{pre_reduce::pre_reduce_book, prune::prune_defs};
use net::{hvmc_to_net::hvmc_to_net, net_to_hvmc::nets_to_hvmc};
use std::time::Instant;
use term::{
  book_to_nets,
  display::{display_readback_errors, DisplayJoin},
  net_to_term::net_to_term,
  term_to_net::{HvmcNames, Labels},
  AdtEncoding, Book, Ctx, ReadbackError, Term,
};

pub mod diagnostics;
pub mod hvmc_net;
pub mod net;
pub mod term;

pub use term::load_book::load_file_to_book;

pub const ENTRY_POINT: &str = "main";
pub const HVM1_ENTRY_POINT: &str = "Main";

pub fn check_book(book: Book) -> Result<(), Info> {
  // TODO: Do the checks without having to do full compilation
  // TODO: Shouldn't the check mode show warnings?
  compile_book(book, CompileOpts::light())?;
  Ok(())
}

pub fn compile_book(book: Book, opts: CompileOpts) -> Result<CompileResult, Info> {
  let (book, warns) = desugar_book(book, opts)?;
  let (nets, hvmc_names, labels) = book_to_nets(&book);
  let mut core_book = nets_to_hvmc(nets, &hvmc_names)?;
  if opts.pre_reduce {
    pre_reduce_book(&mut core_book, opts.pre_reduce_refs, book.hvmc_entrypoint())?;
  }
  if opts.prune {
    prune_defs(&mut core_book, book.hvmc_entrypoint());
  }

  Ok(CompileResult { book, warns, core_book, hvmc_names, labels })
}

pub fn desugar_book(book: Book, opts: CompileOpts) -> Result<(Book, Vec<Warning>), Info> {
  let mut ctx = Ctx::new(book);

  ctx.set_entrypoint();
  ctx.check_shared_names();
  ctx.book.encode_adts(opts.adt_encoding);
  ctx.book.encode_builtins();
  encode_pattern_matching(&mut ctx, opts.adt_encoding)?;
  ctx.check_unbound_vars()?; // sanity check
  ctx.normalize_native_matches()?;
  ctx.check_unbound_vars()?;
  ctx.book.make_var_names_unique();
  ctx.book.linearize_vars();
  ctx.book.eta_reduction(opts.eta);
  ctx.check_unbound_vars()?; // sanity check

  if opts.supercombinators {
    ctx.book.detach_supercombinators();
  }
  if opts.ref_to_ref {
    ctx.simplify_ref_to_ref()?;
  }
  if opts.simplify_main {
    ctx.book.simplify_main_ref();
  }

  ctx.prune(opts.prune, opts.adt_encoding);

  if opts.inline {
    ctx.book.inline();
  }
  if opts.merge {
    ctx.book.merge_definitions();
  }

  if !ctx.info.has_errors() { Ok((ctx.book, ctx.info.warns)) } else { Err(ctx.info) }
}

pub fn encode_pattern_matching(ctx: &mut Ctx, adt_encoding: AdtEncoding) -> Result<(), Info> {
  ctx.check_arity()?;
  ctx.book.resolve_ctrs_in_pats();
  ctx.check_unbound_pats()?;
  ctx.check_ctrs_arities()?;
  ctx.resolve_refs()?;
  ctx.book.desugar_let_destructors();
  ctx.book.desugar_implicit_match_binds();
  // This call to unbound vars needs to be after desugar_implicit_match_binds,
  // since we need the generated pattern names, like `x-1`, `ctr.field`.
  ctx.check_unbound_vars()?;
  ctx.linearize_matches()?;
  ctx.extract_adt_matches()?;
  ctx.book.flatten_rules();
  let def_types = ctx.infer_def_types()?;
  ctx.check_exhaustive_patterns(&def_types)?;
  ctx.book.encode_pattern_matching_functions(&def_types, adt_encoding);
  Ok(())
}

pub fn run_book(
  book: Book,
  mem_size: usize,
  run_opts: RunOpts,
  warning_opts: WarningOpts,
  compile_opts: CompileOpts,
) -> Result<(Term, RunInfo), Info> {
  let CompileResult { book, warns: warnings, core_book, hvmc_names, labels } =
    compile_book(book, compile_opts)?;

  display_warnings(&warnings, warning_opts)?;

  // Run
  let debug_hook = run_opts.debug_hook(&book, &hvmc_names, &labels);
  let (res_lnet, stats) = run_compiled(&core_book, mem_size, run_opts, debug_hook, &book.hvmc_entrypoint());

  // Readback
  let net = hvmc_to_net(&res_lnet, &hvmc_names.hvmc_to_hvml);
  let (mut res_term, mut readback_errors) = net_to_term(&net, &book, &labels, run_opts.linear);
  let resugar_errs = res_term.resugar_adts(&book, compile_opts.adt_encoding);
  res_term.resugar_builtins();

  readback_errors.extend(resugar_errs);
  let info = RunInfo { stats, readback_errors, net: res_lnet };
  Ok((res_term, info))
}

trait Init {
  fn init(mem_size: usize, lazy: bool, entrypoint: &str) -> Self;
}

impl Init for hvmc::run::Net {
  // same code from Net::new but it receives the entrypoint
  fn init(size: usize, lazy: bool, entrypoint: &str) -> Self {
    if lazy {
      let mem = Box::leak(hvmc::run::Heap::<true>::init(size)) as *mut _;
      let net = hvmc::run::NetFields::<true>::new(unsafe { &*mem });
      net.boot(hvmc::ast::name_to_val(entrypoint));
      hvmc::run::Net::Lazy(hvmc::run::StaticNet { mem, net })
    } else {
      let mem = Box::leak(hvmc::run::Heap::<false>::init(size)) as *mut _;
      let net = hvmc::run::NetFields::<false>::new(unsafe { &*mem });
      net.boot(hvmc::ast::name_to_val(entrypoint));
      hvmc::run::Net::Eager(hvmc::run::StaticNet { mem, net })
    }
  }
}

pub fn run_compiled(
  book: &hvmc::ast::Book,
  mem_size: usize,
  run_opts: RunOpts,
  hook: Option<impl FnMut(&Net)>,
  entrypoint: &str,
) -> (Net, RunStats) {
  let runtime_book = book_to_runtime(book);
  let root = &mut hvmc::run::Net::init(mem_size, run_opts.lazy_mode, entrypoint);

  let start_time = Instant::now();

  if let Some(mut hook) = hook {
    expand(root, &runtime_book);
    while !rdex(root).is_empty() {
      hook(&net_from_runtime(root));
      reduce(root, &runtime_book, 1);
      expand(root, &runtime_book);
    }
  } else if run_opts.single_core {
    root.normal(&runtime_book);
  } else {
    root.parallel_normal(&runtime_book);
  }

  let elapsed = start_time.elapsed().as_secs_f64();

  let net = net_from_runtime(root);
  let def = runtime_net_to_runtime_def(root);
  let stats = RunStats { rewrites: root.get_rewrites(), used: def.node.len(), run_time: elapsed };
  (net, stats)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RunOpts {
  pub single_core: bool,
  pub debug: bool,
  pub linear: bool,
  pub lazy_mode: bool,
}

impl RunOpts {
  pub fn lazy() -> Self {
    Self { lazy_mode: true, single_core: true, ..Self::default() }
  }

  fn debug_hook<'a>(
    &'a self,
    book: &'a Book,
    hvmc_names: &'a HvmcNames,
    labels: &'a Labels,
  ) -> Option<impl FnMut(&Net) + 'a> {
    self.debug.then_some({
      |net: &_| {
        let net = hvmc_to_net(net, &hvmc_names.hvmc_to_hvml);
        let (res_term, errors) = net_to_term(&net, book, labels, self.linear);
        println!("{}{}\n---------------------------------------", display_readback_errors(&errors), res_term,)
      }
    })
  }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CompileOpts {
  /// Selects the encoding for the ADT syntax.
  pub adt_encoding: AdtEncoding,

  /// Enables [term::transform::eta_reduction].
  pub eta: bool,

  /// Enables [term::transform::simplify_ref_to_ref].
  pub ref_to_ref: bool,

  /// Enables [term::transform::definition_pruning] and [hvmc_net::prune].
  pub prune: bool,

  /// Enables [hvmc_net::pre_reduce].
  pub pre_reduce: bool,

  /// Enables [term::transform::detach_supercombinators].
  pub supercombinators: bool,

  /// Enables [term::transform::simplify_main_ref].
  pub simplify_main: bool,

  /// Enables dereferences in [hvmc_net::pre_reduce] pass.
  pub pre_reduce_refs: bool,

  /// Enables [term::transform::definition_merge]
  pub merge: bool,

  /// Enables [term::transform::inline].
  pub inline: bool,
}

impl CompileOpts {
  /// All optimizations enabled.
  pub fn heavy() -> Self {
    Self {
      eta: true,
      ref_to_ref: true,
      prune: true,
      pre_reduce: true,
      supercombinators: true,
      simplify_main: true,
      pre_reduce_refs: true,
      merge: true,
      inline: true,
      adt_encoding: Default::default(),
    }
  }

  /// All optimizations disabled, except detach supercombinators.
  pub fn light() -> Self {
    Self { supercombinators: true, ..Self::default() }
  }

  // Disable optimizations that don't work or are unnecessary on lazy mode
  pub fn lazy_mode(&mut self) {
    self.supercombinators = false;
    self.pre_reduce = false;
  }
}

impl CompileOpts {
  pub fn check(&self, lazy_mode: bool) {
    if !self.supercombinators && !lazy_mode {
      println!(
        "Warning: Running in strict mode without enabling the supercombinators pass can lead to some functions expanding infinitely."
      );
    }
  }
}

#[derive(Default, Clone, Copy)]
pub struct WarningOpts {
  pub match_only_vars: WarnState,
  pub unused_defs: WarnState,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WarnState {
  #[default]
  Warn,
  Allow,
  Deny,
}

impl WarningOpts {
  pub fn allow_all() -> Self {
    Self { match_only_vars: WarnState::Allow, unused_defs: WarnState::Allow }
  }

  pub fn deny_all() -> Self {
    Self { match_only_vars: WarnState::Deny, unused_defs: WarnState::Deny }
  }

  pub fn warn_all() -> Self {
    Self { match_only_vars: WarnState::Warn, unused_defs: WarnState::Warn }
  }

  /// Filters warnings based on the enabled flags.
  pub fn filter<'a>(&'a self, warns: &'a [Warning], ws: WarnState) -> Vec<&Warning> {
    warns
      .iter()
      .filter(|w| {
        (match w {
          Warning::MatchOnlyVars(_) => self.match_only_vars,
          Warning::UnusedDefinition(_) => self.unused_defs,
        }) == ws
      })
      .collect()
  }
}

/// Either just prints warnings or returns Err when any denied was produced.
pub fn display_warnings(warnings: &[Warning], warning_opts: WarningOpts) -> Result<(), String> {
  let warns = warning_opts.filter(warnings, WarnState::Warn);
  if !warns.is_empty() {
    eprintln!("Warnings:\n{}", DisplayJoin(|| warns.iter(), "\n"));
  }
  let denies = warning_opts.filter(warnings, WarnState::Deny);
  if !denies.is_empty() {
    return Err(format!(
      "{}\nCould not run the code because of the previous warnings",
      DisplayJoin(|| denies.iter(), "\n")
    ));
  }
  Ok(())
}

pub struct CompileResult {
  pub book: Book,
  pub warns: Vec<Warning>,
  pub core_book: hvmc::ast::Book,
  pub hvmc_names: HvmcNames,
  pub labels: Labels,
}

impl std::fmt::Debug for CompileResult {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    for warn in &self.warns {
      writeln!(f, "// WARNING: {}", warn)?;
    }
    write!(f, "{}", show_book(&self.core_book))
  }
}

impl CompileResult {
  pub fn display_with_warns(&self, opts: WarningOpts) -> Result<String, String> {
    display_warnings(&self.warns, opts)?;
    Ok(show_book(&self.core_book))
  }
}

pub struct RunInfo {
  pub stats: RunStats,
  pub readback_errors: Vec<ReadbackError>,
  pub net: Net,
}

pub struct RunStats {
  pub rewrites: Rewrites,
  pub used: usize,
  pub run_time: f64,
}

fn expand(net: &mut hvmc::run::Net, book: &hvmc::run::Book) {
  match net {
    hvmc::run::Net::Eager(net) => net.net.expand(book),
    _ => unreachable!(),
  }
}

fn reduce(net: &mut hvmc::run::Net, book: &hvmc::run::Book, limit: usize) -> usize {
  match net {
    hvmc::run::Net::Eager(net) => net.net.reduce(book, limit),
    _ => unreachable!(),
  }
}

fn rdex(net: &mut hvmc::run::Net) -> &mut Vec<(hvmc::run::Ptr, hvmc::run::Ptr)> {
  match net {
    hvmc::run::Net::Lazy(net) => &mut net.net.rdex,
    hvmc::run::Net::Eager(net) => &mut net.net.rdex,
  }
}

fn net_from_runtime(net: &hvmc::run::Net) -> Net {
  match net {
    hvmc::run::Net::Lazy(net) => hvmc::ast::net_from_runtime(&net.net),
    hvmc::run::Net::Eager(net) => hvmc::ast::net_from_runtime(&net.net),
  }
}

fn net_to_runtime(rt_net: &mut hvmc::run::Net, net: &Net) {
  match rt_net {
    hvmc::run::Net::Lazy(rt_net) => hvmc::ast::net_to_runtime(&mut rt_net.net, net),
    hvmc::run::Net::Eager(rt_net) => hvmc::ast::net_to_runtime(&mut rt_net.net, net),
  }
}

fn runtime_net_to_runtime_def(net: &hvmc::run::Net) -> Def {
  match net {
    hvmc::run::Net::Lazy(net) => hvmc::ast::runtime_net_to_runtime_def(&net.net),
    hvmc::run::Net::Eager(net) => hvmc::ast::runtime_net_to_runtime_def(&net.net),
  }
}
