//! `#[ferritron::plugin]`: turns the project file (`<name>.ferritroject`) into typed Rust code and
//! exports the plugin for every enabled format.

mod locales;
mod manifest;
mod view;

use proc_macro::{Delimiter, Group, TokenStream, TokenTree};

/// Put this on the `impl Process for MyPlugin` block.
///
/// It reads the project file next to `Cargo.toml` (`<name>.ferritroject` or `<name>.frtx`, or the
/// former `ferritron.toml`) and generates, in the same
/// module: a `Params` struct, a `Context` alias, the plugin metadata and the
/// format entry points.
#[proc_macro_attribute]
pub fn plugin(_attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(item) {
        Ok(tokens) => tokens,
        Err(message) => format!("::core::compile_error!({message:?});").parse().unwrap(),
    }
}

fn expand(item: TokenStream) -> Result<TokenStream, String> {
    let (impl_block, self_ty) = add_params_type(item)?;
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR is not set")?;
    let path = ferritron_manifest::locate(std::path::Path::new(&manifest_dir))?;
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let manifest = manifest::parse(&text).map_err(|e| format!("{file}: {e}"))?;
    let root: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("{file}: {e}"))?;
    let view = view::parse(&root, std::path::Path::new(&manifest_dir)).map_err(|e| format!("{file}: {e}"))?;
    let locales = locales::collect(std::path::Path::new(&manifest_dir), &manifest)?;
    let mut generated = manifest::generate(&manifest, &self_ty, &path.display().to_string());
    let (view_items, view_method) = view.as_ref().map(|v| view::generate(v, &self_ty)).unwrap_or_default();
    generated.push_str(&view_items);
    let parallel = if manifest.parallel { "const PARALLEL: bool = true;" } else { "" };
    let mcp = if manifest.mcp { "const MCP: bool = true;" } else { "" };
    let quality = if manifest.quality { "const QUALITY: bool = true;" } else { "" };
    let ara = if manifest.ara {
        generated.push_str("::ferritron::__require_ara!();\n");
        "fn ara() -> ::core::option::Option<&'static ::ferritron::abi::AraApi> { ::core::option::Option::Some(::ferritron::ara::api::<Self>()) }"
    } else {
        ""
    };
    generated.push_str(&locales::generate(&locales, &self_ty, &format!("{view_method}\n    {parallel}\n    {mcp}\n    {quality}\n    {ara}")));
    let generated: TokenStream = generated.parse().map_err(|e| format!("internal codegen error: {e}"))?;
    let mut out = impl_block;
    out.extend(generated);
    Ok(out)
}

/// Inserts `type Params = Params;` into the impl block unless already present,
/// and returns the implementing type as source text.
fn add_params_type(item: TokenStream) -> Result<(TokenStream, String), String> {
    const USAGE: &str = "#[ferritron::plugin] must be placed on `impl Process for YourPlugin { ... }`";
    let mut tokens: Vec<TokenTree> = item.into_iter().collect();

    let for_pos = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Ident(i) if i.to_string() == "for"))
        .ok_or(USAGE)?;
    let Some(TokenTree::Group(body)) = tokens.last() else { return Err(USAGE.into()) };
    if body.delimiter() != Delimiter::Brace {
        return Err(USAGE.into());
    }
    let self_ty: String = tokens[for_pos + 1..tokens.len() - 1].iter().map(|t| t.to_string()).collect();
    if self_ty.is_empty() {
        return Err(USAGE.into());
    }

    let inner: Vec<TokenTree> = body.stream().into_iter().collect();
    let declares_params = inner
        .windows(2)
        .any(|w| matches!((&w[0], &w[1]), (TokenTree::Ident(a), TokenTree::Ident(b)) if a.to_string() == "type" && b.to_string() == "Params"));
    if !declares_params {
        let mut stream: TokenStream = "type Params = Params;".parse().unwrap();
        stream.extend(inner);
        let span = body.span();
        let mut group = Group::new(Delimiter::Brace, stream);
        group.set_span(span);
        *tokens.last_mut().unwrap() = TokenTree::Group(group);
    }
    Ok((tokens.into_iter().collect(), self_ty))
}
