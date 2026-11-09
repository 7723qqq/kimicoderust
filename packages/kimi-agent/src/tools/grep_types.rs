pub(crate) const RG_FILE_TYPES: &[(&str, &[&str])] = &[
    ("ada", &["*.adb", "*.ads"]),
    ("agda", &["*.agda", "*.lagda"]),
    ("aidl", &["*.aidl"]),
    ("alire", &["alire.toml"]),
    ("amake", &["*.bp", "*.mk"]),
    ("asciidoc", &["*.adoc", "*.asc", "*.asciidoc"]),
    ("asm", &["*.S", "*.asm", "*.s"]),
    (
        "asp",
        &[
            "*.ascx",
            "*.ascx.cs",
            "*.ascx.vb",
            "*.asp",
            "*.aspx",
            "*.aspx.cs",
            "*.aspx.vb",
        ],
    ),
    ("ats", &["*.ats", "*.dats", "*.hats", "*.sats"]),
    ("avro", &["*.avdl", "*.avpr", "*.avsc"]),
    ("awk", &["*.awk"]),
    ("bat", &["*.bat"]),
    ("batch", &["*.bat"]),
    (
        "bazel",
        &[
            "*.BUILD",
            "*.bazel",
            "*.bazelrc",
            "*.bzl",
            "BUILD",
            "MODULE.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
            "WORKSPACE.bzlmod",
        ],
    ),
    (
        "bitbake",
        &["*.bb", "*.bbappend", "*.bbclass", "*.conf", "*.inc"],
    ),
    ("boxlang", &["*.bx", "*.bxm", "*.bxs"]),
    ("brotli", &["*.br"]),
    ("buildstream", &["*.bst"]),
    ("bzip2", &["*.bz2", "*.tbz2"]),
    ("c", &["*.[chH]", "*.[chH].in", "*.cats"]),
    ("cabal", &["*.cabal"]),
    ("candid", &["*.did"]),
    ("carp", &["*.carp"]),
    ("cbor", &["*.cbor"]),
    ("ceylon", &["*.ceylon"]),
    ("cfml", &["*.cfc", "*.cfm"]),
    ("clojure", &["*.clj", "*.cljc", "*.cljs", "*.cljx"]),
    ("cmake", &["*.cmake", "CMakeLists.txt"]),
    ("cmd", &["*.bat", "*.cmd"]),
    ("cml", &["*.cml"]),
    ("coffeescript", &["*.coffee"]),
    ("config", &["*.cfg", "*.conf", "*.config", "*.ini"]),
    ("coq", &["*.v"]),
    (
        "cpp",
        &[
            "*.[ChH]",
            "*.[ChH].in",
            "*.[ch]pp",
            "*.[ch]pp.in",
            "*.[ch]xx",
            "*.[ch]xx.in",
            "*.cc",
            "*.cc.in",
            "*.hh",
            "*.hh.in",
            "*.inl",
        ],
    ),
    ("creole", &["*.creole"]),
    ("crystal", &["*.cr", "*.ecr", "Projectfile", "shard.yml"]),
    ("cs", &["*.cs"]),
    ("csharp", &["*.cs"]),
    ("cshtml", &["*.cshtml"]),
    ("csproj", &["*.csproj"]),
    ("css", &["*.css", "*.scss"]),
    ("csv", &["*.csv"]),
    ("cuda", &["*.cu", "*.cuh"]),
    ("cython", &["*.pxd", "*.pxi", "*.pyx"]),
    ("d", &["*.d"]),
    ("dart", &["*.dart"]),
    ("devicetree", &["*.dts", "*.dtsi", "*.dtso"]),
    ("dhall", &["*.dhall"]),
    ("diff", &["*.diff", "*.patch"]),
    ("dita", &["*.dita", "*.ditamap", "*.ditaval"]),
    ("docker", &["*Dockerfile*"]),
    (
        "dockercompose",
        &["docker-compose.*.yml", "docker-compose.yml"],
    ),
    ("dts", &["*.dts", "*.dtsi"]),
    ("dvc", &["*.dvc", "Dvcfile"]),
    ("ebuild", &["*.ebuild", "*.eclass"]),
    ("edn", &["*.edn"]),
    ("elisp", &["*.el"]),
    (
        "elixir",
        &["*.eex", "*.ex", "*.exs", "*.heex", "*.leex", "*.livemd"],
    ),
    ("elm", &["*.elm"]),
    ("erb", &["*.erb"]),
    ("erlang", &["*.erl", "*.hrl"]),
    ("fennel", &["*.fnl"]),
    ("fidl", &["*.fidl"]),
    ("fish", &["*.fish"]),
    ("flatbuffers", &["*.fbs"]),
    (
        "fortran",
        &[
            "*.F", "*.F77", "*.F90", "*.F95", "*.f", "*.f77", "*.f90", "*.f95", "*.pfo",
        ],
    ),
    ("fsharp", &["*.fs", "*.fsi", "*.fsx"]),
    ("fut", &["*.fut"]),
    ("gap", &["*.g", "*.gap", "*.gd", "*.gi", "*.tst"]),
    ("gdscript", &["*.gd"]),
    ("gleam", &["*.gleam"]),
    ("gn", &["*.gn", "*.gni"]),
    ("go", &["*.go"]),
    ("gprbuild", &["*.gpr"]),
    (
        "gradle",
        &[
            "*.gradle",
            "*.gradle.kts",
            "gradle-wrapper.*",
            "gradle.properties",
            "gradlew",
            "gradlew.bat",
        ],
    ),
    ("graphql", &["*.graphql", "*.graphqls"]),
    ("groovy", &["*.gradle", "*.groovy"]),
    ("gzip", &["*.gz", "*.tgz"]),
    ("h", &["*.h", "*.hh", "*.hpp"]),
    ("haml", &["*.haml"]),
    ("hare", &["*.ha"]),
    ("haskell", &["*.c2hs", "*.cpphs", "*.hs", "*.hsc", "*.lhs"]),
    ("hbs", &["*.hbs"]),
    ("hs", &["*.hs", "*.lhs"]),
    ("html", &["*.ejs", "*.htm", "*.html"]),
    ("hy", &["*.hy"]),
    ("idris", &["*.idr", "*.lidr"]),
    ("janet", &["*.janet"]),
    ("java", &["*.java", "*.jsp", "*.jspx", "*.properties"]),
    ("jinja", &["*.j2", "*.jinja", "*.jinja2"]),
    ("jl", &["*.jl"]),
    ("js", &["*.cjs", "*.js", "*.jsx", "*.mjs", "*.vue"]),
    ("json", &["*.json", "*.sarif", "composer.lock"]),
    ("jsonl", &["*.jsonl"]),
    ("julia", &["*.jl"]),
    ("jupyter", &["*.ipynb", "*.jpynb"]),
    ("k", &["*.k"]),
    ("kconfig", &["Kconfig", "Kconfig.*"]),
    ("kotlin", &["*.kt", "*.kts"]),
    ("lean", &["*.lean"]),
    ("less", &["*.less"]),
    (
        "license",
        &[
            "*[.-]LICEN[CS]E*",
            "AGPL-*[0-9]*",
            "APACHE-*[0-9]*",
            "BSD-*[0-9]*",
            "CC-BY-*",
            "COPYING",
            "COPYING[.-]*",
            "COPYRIGHT",
            "COPYRIGHT[.-]*",
            "EULA",
            "EULA[.-]*",
            "GFDL-*[0-9]*",
            "GNU-*[0-9]*",
            "GPL-*[0-9]*",
            "LGPL-*[0-9]*",
            "LICEN[CS]E",
            "LICEN[CS]E[.-]*",
            "MIT-*[0-9]*",
            "MPL-*[0-9]*",
            "NOTICE",
            "NOTICE[.-]*",
            "OFL-*[0-9]*",
            "PATENTS",
            "PATENTS[.-]*",
            "UNLICEN[CS]E",
            "UNLICEN[CS]E[.-]*",
            "agpl[.-]*",
            "gpl[.-]*",
            "lgpl[.-]*",
            "licen[cs]e",
            "licen[cs]e.*",
        ],
    ),
    ("lilypond", &["*.ily", "*.ly"]),
    (
        "lisp",
        &["*.el", "*.jl", "*.lisp", "*.lsp", "*.sc", "*.scm"],
    ),
    ("llvm", &["*.ll"]),
    ("lock", &["*.lock", "package-lock.json"]),
    ("log", &["*.log"]),
    ("lua", &["*.lua"]),
    ("lz4", &["*.lz4"]),
    ("lzma", &["*.lzma"]),
    ("m4", &["*.ac", "*.m4"]),
    (
        "make",
        &[
            "*.mak",
            "*.mk",
            "Makefile.*",
            "[Gg][Nn][Uu]makefile",
            "[Gg][Nn][Uu]makefile.am",
            "[Gg][Nn][Uu]makefile.in",
            "[Mm]akefile",
            "[Mm]akefile.am",
            "[Mm]akefile.in",
        ],
    ),
    ("mako", &["*.mako", "*.mao"]),
    ("man", &["*.[0-9][cEFMmpSx]", "*.[0-9lnpx]"]),
    (
        "markdown",
        &[
            "*.markdown",
            "*.md",
            "*.mdown",
            "*.mdwn",
            "*.mdx",
            "*.mkd",
            "*.mkdn",
        ],
    ),
    ("matlab", &["*.m"]),
    (
        "md",
        &[
            "*.markdown",
            "*.md",
            "*.mdown",
            "*.mdwn",
            "*.mdx",
            "*.mkd",
            "*.mkdn",
        ],
    ),
    (
        "meson",
        &["meson.build", "meson.options", "meson_options.txt"],
    ),
    ("minified", &["*.min.css", "*.min.html", "*.min.js"]),
    ("mint", &["*.mint"]),
    ("mk", &["mkfile"]),
    ("ml", &["*.ml"]),
    ("motoko", &["*.mo"]),
    (
        "msbuild",
        &[
            "*.csproj",
            "*.fsproj",
            "*.proj",
            "*.props",
            "*.sln",
            "*.slnf",
            "*.targets",
            "*.vcxproj",
        ],
    ),
    ("nim", &["*.nim", "*.nimble", "*.nimf", "*.nims"]),
    ("nix", &["*.nix"]),
    ("objc", &["*.h", "*.m"]),
    ("objcpp", &["*.h", "*.mm"]),
    ("ocaml", &["*.ml", "*.mli", "*.mll", "*.mly"]),
    ("org", &["*.org", "*.org_archive"]),
    ("pants", &["BUILD"]),
    ("pascal", &["*.dpr", "*.inc", "*.lpr", "*.pas", "*.pp"]),
    ("pdf", &["*.pdf"]),
    (
        "perl",
        &["*.PL", "*.perl", "*.pl", "*.plh", "*.plx", "*.pm", "*.t"],
    ),
    (
        "php",
        &[
            "*.php", "*.php3", "*.php4", "*.php5", "*.php7", "*.php8", "*.pht", "*.phtml",
        ],
    ),
    ("po", &["*.po"]),
    ("pod", &["*.pod"]),
    ("postscript", &["*.eps", "*.ps"]),
    ("prolog", &["*.P", "*.pl", "*.pro", "*.prolog"]),
    ("protobuf", &["*.proto"]),
    ("ps", &["*.cdxml", "*.ps1", "*.ps1xml", "*.psd1", "*.psm1"]),
    ("puppet", &["*.epp", "*.erb", "*.pp", "*.rb"]),
    ("purs", &["*.purs"]),
    ("py", &["*.py", "*.pyi"]),
    ("python", &["*.py", "*.pyi"]),
    ("qmake", &["*.prf", "*.pri", "*.pro"]),
    ("qml", &["*.qml"]),
    ("qrc", &["*.qrc"]),
    ("qui", &["*.ui"]),
    ("r", &["*.R", "*.Rmd", "*.Rnw", "*.r", "*.rmd", "*.rnw"]),
    ("racket", &["*.rkt"]),
    (
        "raku",
        &[
            "*.p6",
            "*.pl6",
            "*.pm6",
            "*.raku",
            "*.rakudoc",
            "*.rakumod",
            "*.rakutest",
        ],
    ),
    ("rdoc", &["*.rdoc"]),
    ("readme", &["*README", "README*"]),
    ("reasonml", &["*.re", "*.rei"]),
    ("red", &["*.r", "*.red", "*.reds"]),
    ("rescript", &["*.res", "*.resi"]),
    ("robot", &["*.robot"]),
    ("rst", &["*.rst"]),
    (
        "ruby",
        &[
            "*.gemspec",
            "*.rake",
            "*.rb",
            "*.rbw",
            ".irbrc",
            "Gemfile",
            "Rakefile",
            "config.ru",
        ],
    ),
    ("rust", &["*.rs"]),
    ("sass", &["*.sass", "*.scss"]),
    ("scala", &["*.sbt", "*.scala"]),
    ("scdoc", &["*.scd", "*.scdoc"]),
    ("seed7", &["*.s7i", "*.sd7"]),
    (
        "sh",
        &[
            "*.bash",
            "*.bashrc",
            "*.csh",
            "*.cshrc",
            "*.env",
            "*.ksh",
            "*.kshrc",
            "*.sh",
            "*.tcsh",
            "*.zsh",
            ".bash_login",
            ".bash_logout",
            ".bash_profile",
            ".bashrc",
            ".cshrc",
            ".env",
            ".kshrc",
            ".login",
            ".logout",
            ".profile",
            ".tcshrc",
            ".zlogin",
            ".zlogout",
            ".zprofile",
            ".zshenv",
            ".zshrc",
            "bash_login",
            "bash_logout",
            "bash_profile",
            "bashrc",
            "profile",
            "zlogin",
            "zlogout",
            "zprofile",
            "zshenv",
            "zshrc",
        ],
    ),
    ("slim", &["*.skim", "*.slim", "*.slime"]),
    ("smarty", &["*.tpl"]),
    ("sml", &["*.sig", "*.sml"]),
    ("solidity", &["*.sol"]),
    ("soy", &["*.soy"]),
    ("spark", &["*.spark"]),
    ("spec", &["*.spec"]),
    ("sql", &["*.psql", "*.sql"]),
    ("stylus", &["*.styl"]),
    ("sv", &["*.h", "*.sv", "*.svh", "*.v", "*.vg"]),
    ("svelte", &["*.svelte", "*.svelte.ts"]),
    ("svg", &["*.svg"]),
    ("swift", &["*.swift"]),
    ("swig", &["*.def", "*.i"]),
    (
        "systemd",
        &[
            "*.automount",
            "*.conf",
            "*.device",
            "*.link",
            "*.mount",
            "*.path",
            "*.scope",
            "*.service",
            "*.slice",
            "*.socket",
            "*.swap",
            "*.target",
            "*.timer",
        ],
    ),
    ("taskpaper", &["*.taskpaper"]),
    ("tcl", &["*.tcl"]),
    (
        "tex",
        &[
            "*.bib", "*.cls", "*.dtx", "*.ins", "*.ltx", "*.sty", "*.tex",
        ],
    ),
    ("texinfo", &["*.texi"]),
    ("textile", &["*.textile"]),
    (
        "tf",
        &[
            "*.terraform.lock.hcl",
            "*.terraformrc",
            "*.tf",
            "*.tf.json",
            "*.tfrc",
            "*.tfvars",
            "*.tfvars.json",
            "terraform.rc",
        ],
    ),
    ("thrift", &["*.thrift"]),
    ("toml", &["*.toml", "Cargo.lock"]),
    ("ts", &["*.cts", "*.mts", "*.ts", "*.tsx"]),
    ("twig", &["*.twig"]),
    ("txt", &["*.txt"]),
    ("typescript", &["*.cts", "*.mts", "*.ts", "*.tsx"]),
    ("typoscript", &["*.ts", "*.typoscript"]),
    ("typst", &["*.typ"]),
    ("usd", &["*.usd", "*.usda", "*.usdc"]),
    ("v", &["*.v", "*.vsh"]),
    ("vala", &["*.vala"]),
    ("vb", &["*.vb"]),
    ("vcl", &["*.vcl"]),
    ("verilog", &["*.sv", "*.svh", "*.v", "*.vh"]),
    ("vhdl", &["*.vhd", "*.vhdl"]),
    (
        "vim",
        &[
            "*.vim", ".gvimrc", ".vimrc", "_gvimrc", "_vimrc", "gvimrc", "vimrc",
        ],
    ),
    (
        "vimscript",
        &[
            "*.vim", ".gvimrc", ".vimrc", "_gvimrc", "_vimrc", "gvimrc", "vimrc",
        ],
    ),
    ("vue", &["*.vue"]),
    ("webidl", &["*.idl", "*.webidl", "*.widl"]),
    ("wgsl", &["*.wgsl"]),
    ("wiki", &["*.mediawiki", "*.wiki"]),
    (
        "xml",
        &[
            "*.dtd",
            "*.rng",
            "*.sch",
            "*.xhtml",
            "*.xjb",
            "*.xml",
            "*.xml.dist",
            "*.xsd",
            "*.xsl",
            "*.xslt",
        ],
    ),
    ("xz", &["*.txz", "*.xz"]),
    ("yacc", &["*.y"]),
    ("yaml", &["*.yaml", "*.yml"]),
    ("yang", &["*.yang"]),
    ("z", &["*.Z"]),
    ("zig", &["*.zig"]),
    (
        "zsh",
        &[
            "*.zsh",
            ".zlogin",
            ".zlogout",
            ".zprofile",
            ".zshenv",
            ".zshrc",
            "zlogin",
            "zlogout",
            "zprofile",
            "zshenv",
            "zshrc",
        ],
    ),
    ("zstd", &["*.zst", "*.zstd"]),
];

pub(crate) fn rg_type_globs(name: &str) -> Option<&'static [&'static str]> {
    RG_FILE_TYPES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, globs)| *globs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn lookup_is_case_sensitive_like_ripgrep() {
        assert_eq!(rg_type_globs("rust"), Some(["*.rs"].as_slice()));

        assert_eq!(rg_type_globs("Rust"), None);
        assert_eq!(rg_type_globs(""), None);
        assert_eq!(rg_type_globs("kimiunknown"), None);
    }

    #[test]
    fn table_is_sorted_and_free_of_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        let mut previous: Option<&str> = None;
        for (name, globs) in RG_FILE_TYPES {
            assert!(
                !globs.is_empty(),
                "type {name} must carry at least one glob"
            );
            assert!(seen.insert(*name), "duplicate type name: {name}");
            if let Some(previous) = previous {
                assert!(
                    previous < *name,
                    "type table must be sorted by name: {previous} before {name}"
                );
            }
            previous = Some(name);
        }

        assert_eq!(RG_FILE_TYPES.len(), 217, "type count drifted");
    }

    #[test]
    fn table_agrees_with_the_local_ripgrep_type_list() {
        let Some(listing) = run_ripgrep_type_list() else {
            eprintln!("skipping: no rg binary reachable from this test");
            return;
        };
        let ours: BTreeMap<&str, &[&str]> = RG_FILE_TYPES
            .iter()
            .map(|(name, globs)| (*name, *globs))
            .collect();

        let mut mismatched: Vec<String> = Vec::new();
        let mut matched = 0usize;
        for (name, globs) in &ours {
            match listing.get(*name) {
                Some(theirs) if same_globs(theirs, globs) => matched += 1,
                Some(theirs) => {
                    mismatched.push(format!("{name}: engine {:?} != rg {:?}", *globs, theirs))
                }
                None => mismatched.push(format!("{name}: absent from rg --type-list")),
            }
        }
        assert!(
            mismatched.is_empty(),
            "{} of {} types disagree with the local rg:\n{}",
            mismatched.len(),
            ours.len(),
            mismatched.join("\n")
        );
        assert_eq!(matched, ours.len());

        let rg_only = listing.len().saturating_sub(matched);
        if rg_only > 0 {
            eprintln!("note: {rg_only} rg type(s) are not in the engine table");
        }
    }

    fn same_globs(theirs: &[String], ours: &[&str]) -> bool {
        theirs.len() == ours.len()
            && theirs
                .iter()
                .zip(ours.iter())
                .all(|(a, b)| a.as_str() == *b)
    }

    fn run_ripgrep_type_list() -> Option<BTreeMap<String, Vec<String>>> {
        let output = std::process::Command::new("rg")
            .arg("--type-list")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut parsed = BTreeMap::new();
        for line in stdout.lines() {
            let Some((name, globs)) = line.split_once(':') else {
                continue;
            };
            let globs: Vec<String> = globs
                .split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect();
            if !globs.is_empty() {
                parsed.insert(name.trim().to_string(), globs);
            }
        }
        (!parsed.is_empty()).then_some(parsed)
    }
}
