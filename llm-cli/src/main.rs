use std::{convert::Infallible, io::Write};

use clap::Parser;
use cli_args::Args;
use color_eyre::eyre::{Context, Result};
use llm::{llama::convert::convert_pth_to_ggml, snapshot, InferenceError};
use rustyline::error::ReadlineError;

mod cli_args;

fn main() -> Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    color_eyre::install()?;

    let cli_args = Args::parse();
    match cli_args {
        Args::Infer(args) => infer(&args)?,
        Args::DumpTokens(args) => dump_tokens(&args)?,
        Args::Repl(args) => interactive(&args, false)?,
        Args::ChatExperimental(args) => interactive(&args, true)?,
        Args::Convert(args) => convert_pth_to_ggml(&args.directory, args.file_type.into()),
        Args::Quantize(args) => quantize(&args)?,
    }

    Ok(())
}

fn infer(args: &cli_args::Infer) -> Result<()> {
    let prompt = load_prompt_file_with_prompt(&args.prompt_file, args.prompt.as_deref());
    let inference_session_params = args.generate.inference_session_parameters();
    let model = args.model_load.load()?;
    let (mut session, session_loaded) = snapshot::read_or_create_session(
        model.as_ref(),
        args.persist_session.as_deref(),
        args.generate.load_session.as_deref(),
        inference_session_params,
    );
    let inference_params = args.generate.inference_parameters(session_loaded);

    let mut rng = args.generate.rng();
    let res = session.inference_with_prompt::<Infallible>(
        model.as_ref(),
        &inference_params,
        &prompt,
        args.generate.num_predict,
        &mut rng,
        |t| {
            print!("{t}");
            std::io::stdout().flush().unwrap();

            Ok(())
        },
    );
    println!();

    match res {
        Ok(_) => (),
        Err(InferenceError::ContextFull) => {
            log::warn!("Context window full, stopping inference.")
        }
        Err(InferenceError::TokenizationFailed) => {
            log::error!("Failed to tokenize initial prompt.");
        }
        Err(InferenceError::UserCallback(_)) | Err(InferenceError::EndOfText) => {
            unreachable!("cannot fail")
        }
    }

    if let Some(session_path) = args.save_session.as_ref().or(args.persist_session.as_ref()) {
        // Write the memory to the cache file
        snapshot::write_session(session, session_path);
    }

    Ok(())
}

fn dump_tokens(args: &cli_args::DumpTokens) -> Result<()> {
    let prompt = load_prompt_file_with_prompt(&args.prompt_file, args.prompt.as_deref());
    let model = args.model_load.load()?;
    let toks = match model.vocabulary().tokenize(&prompt, false) {
        Ok(toks) => toks,
        Err(e) => {
            log::error!("Could not tokenize prompt: {e}");
            std::process::exit(1);
        }
    };
    log::info!("=== Dumping prompt tokens:");
    log::info!(
        "{}",
        toks.iter()
            .map(|(_, tid)| tid.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    log::info!(
        "{}",
        toks.iter()
            .map(|(s, tid)| format!("{s:?}:{tid}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(())
}

fn interactive(
    args: &cli_args::Repl,
    // If set to false, the session will be cloned after each inference
    // to ensure that previous state is not carried over.
    chat_mode: bool,
) -> Result<()> {
    let prompt_file = args.prompt_file.contents();
    let inference_session_params = args.generate.inference_session_parameters();
    let model = args.model_load.load()?;
    let (mut session, session_loaded) = snapshot::read_or_create_session(
        model.as_ref(),
        None,
        args.generate.load_session.as_deref(),
        inference_session_params,
    );
    let inference_params = args.generate.inference_parameters(session_loaded);

    let mut rng = args.generate.rng();
    let mut rl = rustyline::DefaultEditor::new()?;
    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let session_backup = if chat_mode {
                    None
                } else {
                    Some(session.clone())
                };

                let prompt = prompt_file
                    .as_deref()
                    .map(|pf| process_prompt(pf, &line))
                    .unwrap_or(line);

                let mut sp = spinners::Spinner::new(spinners::Spinners::Dots2, "".to_string());
                if let Err(InferenceError::ContextFull) = session.feed_prompt::<Infallible>(
                    model.as_ref(),
                    &inference_params,
                    &prompt,
                    |_| Ok(()),
                ) {
                    log::error!("Prompt exceeds context window length.")
                };
                sp.stop();

                let res = session.inference_with_prompt::<Infallible>(
                    model.as_ref(),
                    &inference_params,
                    "",
                    args.generate.num_predict,
                    &mut rng,
                    |tk| {
                        print!("{tk}");
                        std::io::stdout().flush().unwrap();
                        Ok(())
                    },
                );
                println!();

                if let Err(InferenceError::ContextFull) = res {
                    log::error!("Reply exceeds context window length");
                }

                if let Some(session_backup) = session_backup {
                    session = session_backup;
                }
            }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => {
                break;
            }
            Err(err) => {
                log::error!("{err}");
            }
        }
    }

    Ok(())
}

fn quantize(args: &cli_args::Quantize) -> Result<()> {
    use llm::llama::quantize::{quantize, QuantizeProgress::*};
    quantize(
        &args.source,
        &args.destination,
        args.target.into(),
        |progress| match progress {
            HyperparametersLoaded => log::info!("Loaded hyperparameters"),
            TensorLoading {
                name,
                dims,
                element_type,
                n_elements,
            } => log::info!(
                "Loading tensor `{name}` ({n_elements} ({dims:?}) {element_type} elements)"
            ),
            TensorQuantizing { name } => log::info!("Quantizing tensor `{name}`"),
            TensorQuantized {
                name,
                original_size,
                reduced_size,
                history,
            } => log::info!(
            "Quantized tensor `{name}` from {original_size} to {reduced_size} bytes ({history:?})"
        ),
            TensorSkipped { name, size } => log::info!("Skipped tensor `{name}` ({size} bytes)"),
            Finished {
                original_size,
                reduced_size,
                history,
            } => log::info!(
                "Finished quantization from {original_size} to {reduced_size} bytes ({history:?})"
            ),
        },
    )
    .wrap_err("failed to quantize model")
}

fn load_prompt_file_with_prompt(
    prompt_file: &cli_args::PromptFile,
    prompt: Option<&str>,
) -> String {
    if let Some(prompt_file) = prompt_file.contents() {
        if let Some(prompt) = prompt {
            process_prompt(&prompt_file, prompt)
        } else {
            prompt_file
        }
    } else if let Some(prompt) = prompt {
        prompt.to_owned()
    } else {
        log::error!("No prompt or prompt file was provided. See --help");
        std::process::exit(1);
    }
}

fn process_prompt(raw_prompt: &str, prompt: &str) -> String {
    raw_prompt.replace("{{PROMPT}}", prompt)
}