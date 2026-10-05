//! Audit explicit hotpath hooks using Rust syntax, including inactive platforms.
//! Run from any directory. --instrument adds missing attributes; default checks.
use std::{
    fs,
    path::{Path, PathBuf},
};

use quote::ToTokens;
use serde::Serialize;
use syn::{
    Attribute, Signature,
    spanned::Spanned,
    visit::{self, Visit},
};

#[derive(Serialize)]
struct Entry {
    file: String,
    line: usize,
    name: String,
    status: &'static str,
    asynchronous: bool,
}

struct Audit {
    file: String,
    module: String,
    owner: Option<String>,
    entries: Vec<Entry>,
    insertions: Vec<(usize, String)>,
}

fn test_only(attrs: &[Attribute]) -> bool {
    fn condition(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(p) => p.is_ident("test") || p.is_ident("kani"),
            syn::Meta::List(list) => {
                let Ok(items) = list.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                ) else {
                    return false;
                };
                if list.path.is_ident("all") {
                    items.iter().any(condition)
                } else if list.path.is_ident("any") {
                    !items.is_empty() && items.iter().all(condition)
                } else {
                    false
                }
            }
            _ => false,
        }
    }
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>().is_ok_and(|m| condition(&m)))
    })
}

impl Audit {
    fn function(&mut self, sig: &Signature, attrs: &[Attribute], line: usize) {
        if test_only(attrs) {
            return;
        }
        let method = self
            .owner
            .as_ref()
            .map(|s| format!("{s}::"))
            .unwrap_or_default();
        let name = format!("{}::{method}{}", self.module, sig.ident);
        let measured = attrs.iter().any(|a| {
            let text = a.meta.to_token_stream().to_string();
            text.contains("hotpath :: measure")
        });
        let mut status = if sig.constness.is_some() {
            "const_fn"
        } else if sig.abi.is_some() {
            "ffi_callback"
        } else if self.file == "src/profile.rs" {
            "profiler_control"
        } else if self.file == "src/bindings/loop_api/pre_exec.rs" {
            "post_fork"
        } else if self.file == "src/vibeio/signal/unix.rs"
            && (sig.ident == "write_signal_notification"
                || (self.owner.as_deref() == Some("PendingSignals") && sig.ident == "notify"))
        {
            "signal_safe"
        } else if sig.ident == "__traverse__" {
            // A CPython GC visitor must not allocate or run arbitrary callbacks.
            "gc_visitor"
        } else if measured {
            "instrumented"
        } else {
            let mut args = Vec::new();
            if let Some(owner) = &self.owner {
                args.push(format!("impl_type = {owner:?}"));
            }
            if sig.asyncness.is_some() {
                args.push("future = true".into());
            }
            let args = if args.is_empty() {
                String::new()
            } else {
                format!("({})", args.join(", "))
            };
            self.insertions.push((
                line,
                format!("#[cfg_attr(feature = \"profile\", hotpath::measure{args})]"),
            ));
            "missing"
        };
        if measured && status != "instrumented" {
            status = "forbidden_hook";
        }
        if measured
            && sig.asyncness.is_some()
            && !attrs.iter().any(|a| {
                a.meta
                    .to_token_stream()
                    .to_string()
                    .contains("future = true")
            })
        {
            status = "missing_future_tracking";
        }
        self.entries.push(Entry {
            file: self.file.clone(),
            line: sig.ident.span().start().line,
            name,
            status,
            asynchronous: sig.asyncness.is_some(),
        });
    }
}

impl<'ast> Visit<'ast> for Audit {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if test_only(&node.attrs) {
            return;
        }
        let old = self.module.clone();
        self.module.push_str(&format!("::{}", node.ident));
        visit::visit_item_mod(self, node);
        self.module = old;
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if test_only(&node.attrs) {
            return;
        }
        let ty = match node.self_ty.as_ref() {
            syn::Type::Path(p) => p.path.segments.last().unwrap().ident.to_string(),
            other => other.to_token_stream().to_string(),
        };
        let owner = if let Some((_, path, _)) = &node.trait_ {
            format!("<{ty} as {}>", path.to_token_stream())
        } else {
            ty
        };
        let old = self.owner.replace(owner);
        visit::visit_item_impl(self, node);
        self.owner = old;
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.function(&node.sig, &node.attrs, node.span().start().line);
        // Nested helpers are separately inventoried, but have module-scoped labels.
        if !test_only(&node.attrs) {
            visit::visit_block(self, &node.block);
        }
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.function(&node.sig, &node.attrs, node.span().start().line);
        if !test_only(&node.attrs) {
            visit::visit_block(self, &node.block);
        }
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let old = self.owner.replace(node.ident.to_string());
        visit::visit_item_trait(self, node);
        self.owner = old;
    }
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if node.default.is_some() {
            self.function(&node.sig, &node.attrs, node.span().start().line);
        }
    }
}

fn sources(dir: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            sources(&path, paths)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            paths.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|s| s != "--instrument" && s != "--json") {
        return Err("usage: rsloop-hotpath-coverage [--instrument] [--json]".into());
    }
    let instrument = args.iter().any(|s| s == "--instrument");
    let mut paths = Vec::new();
    sources(&root.join("src"), &mut paths)?;
    paths.sort();
    let mut entries = Vec::new();
    let mut missing = 0;
    for path in paths {
        let file = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        if file == "src/verification.rs"
            || file.ends_with("/test_support.rs")
            || file.ends_with("/mock.rs")
        {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        let syntax = syn::parse_file(&source)?;
        let mut parts: Vec<_> = file.trim_end_matches(".rs").split('/').skip(1).collect();
        if matches!(parts.last(), Some(&"mod" | &"lib")) {
            parts.pop();
        }
        let module = std::iter::once("rsloop")
            .chain(parts)
            .collect::<Vec<_>>()
            .join("::");
        let mut audit = Audit {
            file,
            module,
            owner: None,
            entries: Vec::new(),
            insertions: Vec::new(),
        };
        audit.visit_file(&syntax);
        missing += audit.insertions.len();
        if instrument && !audit.insertions.is_empty() {
            let mut lines: Vec<String> = source.lines().map(str::to_string).collect();
            audit.insertions.sort_by_key(|(line, _)| *line);
            for (line, attr) in audit.insertions.into_iter().rev() {
                let indent: String = lines[line - 1]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                lines.insert(line - 1, format!("{indent}{attr}"));
            }
            fs::write(&path, lines.join("\n") + "\n")?;
        }
        entries.extend(audit.entries);
    }
    if args.iter().any(|s| s == "--json") {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    }
    eprintln!(
        "{} function definitions audited; {missing} {} hooks",
        entries.len(),
        if instrument { "added" } else { "missing" }
    );
    let invalid: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.status, "forbidden_hook" | "missing_future_tracking"))
        .collect();
    for entry in &invalid {
        eprintln!(
            "{}:{}: {} ({})",
            entry.file, entry.line, entry.status, entry.name
        );
    }
    if (missing > 0 && !instrument) || !invalid.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(source: &str, file: &str) -> Audit {
        let mut audit = Audit {
            file: file.into(),
            module: "rsloop::fixture".into(),
            owner: None,
            entries: Vec::new(),
            insertions: Vec::new(),
        };
        audit.visit_file(&syn::parse_file(source).unwrap());
        audit
    }

    #[test]
    fn skips_tests_but_keeps_optional_runtime_code() {
        let a = audit(
            r#"
            #[cfg(all(unix, test))] mod tests { fn helper() {} }
            #[cfg(any(test, feature = "fs"))] fn optional() {}
            #[cfg(any(test, kani))] fn proof() {}
            const fn constant() -> u8 { 1 }
            extern "C" fn callback() {}
        "#,
            "src/example.rs",
        );
        assert_eq!(a.insertions.len(), 1);
        assert_eq!(
            a.entries.iter().map(|e| e.status).collect::<Vec<_>>(),
            ["missing", "const_fn", "ffi_callback"]
        );
    }

    #[test]
    fn methods_keep_type_and_trait_identity_and_async_tracking() {
        let a = audit(
            "impl A { async fn run() {} } impl Future for A { fn poll() {} }",
            "src/example.rs",
        );
        assert!(
            a.insertions[0]
                .1
                .contains("impl_type = \"A\", future = true")
        );
        assert_ne!(a.entries[0].name, a.entries[1].name);
        assert!(a.entries[1].name.contains("<A as Future>"));
    }

    #[test]
    fn rejects_hooks_on_signal_safe_helpers_and_missing_future_tracking() {
        let a = audit(
            r#"
            #[cfg_attr(feature = "profile", hotpath::measure)]
            fn write_signal_notification() {}
            #[cfg_attr(feature = "profile", hotpath::measure)]
            async fn receive() {}
        "#,
            "src/vibeio/signal/unix.rs",
        );
        assert_eq!(a.entries[0].status, "forbidden_hook");
        assert_eq!(a.entries[1].status, "missing_future_tracking");
        assert!(a.insertions.is_empty());
    }
}
