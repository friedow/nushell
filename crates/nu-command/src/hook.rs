use crate::util::{get_guaranteed_cwd, report_error, report_error_new};
use miette::Result;
use nu_engine::{eval_block, eval_block_with_early_return};
use nu_parser::parse;
use nu_protocol::ast::PathMember;
use nu_protocol::engine::{EngineState, Stack, StateWorkingSet};
use nu_protocol::{BlockId, PipelineData, PositionalArg, ShellError, Span, Type, Value, VarId};

pub fn eval_env_change_hook(
    env_change_hook: Option<Value>,
    engine_state: &mut EngineState,
    stack: &mut Stack,
) -> Result<(), ShellError> {
    if let Some(hook) = env_change_hook {
        match hook {
            Value::Record {
                cols: env_names,
                vals: hook_values,
                ..
            } => {
                for (env_name, hook_value) in env_names.iter().zip(hook_values.iter()) {
                    let before = engine_state
                        .previous_env_vars
                        .get(env_name)
                        .cloned()
                        .unwrap_or_default();

                    let after = stack
                        .get_env_var(engine_state, env_name)
                        .unwrap_or_default();

                    if before != after {
                        eval_hook(
                            engine_state,
                            stack,
                            None,
                            vec![("$before".into(), before), ("$after".into(), after.clone())],
                            hook_value,
                        )?;

                        engine_state
                            .previous_env_vars
                            .insert(env_name.to_string(), after);
                    }
                }
            }
            x => {
                return Err(ShellError::TypeMismatch {
                    err_message: "record for the 'env_change' hook".to_string(),
                    span: x.span()?,
                });
            }
        }
    }

    Ok(())
}

pub fn eval_hook(
    engine_state: &mut EngineState,
    stack: &mut Stack,
    input: Option<PipelineData>,
    arguments: Vec<(String, Value)>,
    value: &Value,
) -> Result<PipelineData, ShellError> {
    let value_span = value.span()?;

    // Hooks can optionally be a record in this form:
    // {
    //     condition: {|before, after| ... }  # block that evaluates to true/false
    //     code: # block or a string
    // }
    // The condition block will be run to check whether the main hook (in `code`) should be run.
    // If it returns true (the default if a condition block is not specified), the hook should be run.
    let condition_path = PathMember::String {
        val: "condition".to_string(),
        span: value_span,
        optional: false,
    };
    let mut output = PipelineData::empty();

    let code_path = PathMember::String {
        val: "code".to_string(),
        span: value_span,
        optional: false,
    };

    match value {
        Value::List { vals, .. } => {
            for val in vals {
                eval_hook(engine_state, stack, None, arguments.clone(), val)?;
            }
        }
        Value::Record { .. } => {
            let do_run_hook =
                if let Ok(condition) = value.clone().follow_cell_path(&[condition_path], false) {
                    match condition {
                        Value::Block {
                            val: block_id,
                            span: block_span,
                            ..
                        }
                        | Value::Closure {
                            val: block_id,
                            span: block_span,
                            ..
                        } => {
                            match run_hook_block(
                                engine_state,
                                stack,
                                block_id,
                                None,
                                arguments.clone(),
                                block_span,
                            ) {
                                Ok(pipeline_data) => {
                                    if let PipelineData::Value(Value::Bool { val, .. }, ..) =
                                        pipeline_data
                                    {
                                        val
                                    } else {
                                        return Err(ShellError::UnsupportedConfigValue(
                                            "boolean output".to_string(),
                                            "other PipelineData variant".to_string(),
                                            block_span,
                                        ));
                                    }
                                }
                                Err(err) => {
                                    return Err(err);
                                }
                            }
                        }
                        other => {
                            return Err(ShellError::UnsupportedConfigValue(
                                "block".to_string(),
                                format!("{}", other.get_type()),
                                other.span()?,
                            ));
                        }
                    }
                } else {
                    // always run the hook
                    true
                };

            if do_run_hook {
                match value.clone().follow_cell_path(&[code_path], false)? {
                    Value::String {
                        val,
                        span: source_span,
                    } => {
                        let (block, delta, vars) = {
                            let mut working_set = StateWorkingSet::new(engine_state);

                            let mut vars: Vec<(VarId, Value)> = vec![];

                            for (name, val) in arguments {
                                let var_id = working_set.add_variable(
                                    name.as_bytes().to_vec(),
                                    val.span()?,
                                    Type::Any,
                                    false,
                                );

                                vars.push((var_id, val));
                            }

                            let (output, err) =
                                parse(&mut working_set, Some("hook"), val.as_bytes(), false, &[]);
                            if let Some(err) = err {
                                report_error(&working_set, &err);

                                return Err(ShellError::UnsupportedConfigValue(
                                    "valid source code".into(),
                                    "source code with syntax errors".into(),
                                    source_span,
                                ));
                            }

                            (output, working_set.render(), vars)
                        };

                        engine_state.merge_delta(delta)?;
                        let input = PipelineData::empty();

                        let var_ids: Vec<VarId> = vars
                            .into_iter()
                            .map(|(var_id, val)| {
                                stack.add_var(var_id, val);
                                var_id
                            })
                            .collect();

                        match eval_block(engine_state, stack, &block, input, false, false) {
                            Ok(pipeline_data) => {
                                output = pipeline_data;
                            }
                            Err(err) => {
                                report_error_new(engine_state, &err);
                            }
                        }

                        for var_id in var_ids.iter() {
                            stack.vars.remove(var_id);
                        }
                    }
                    Value::Block {
                        val: block_id,
                        span: block_span,
                        ..
                    } => {
                        run_hook_block(
                            engine_state,
                            stack,
                            block_id,
                            input,
                            arguments,
                            block_span,
                        )?;
                    }
                    Value::Closure {
                        val: block_id,
                        span: block_span,
                        ..
                    } => {
                        run_hook_block(
                            engine_state,
                            stack,
                            block_id,
                            input,
                            arguments,
                            block_span,
                        )?;
                    }
                    other => {
                        return Err(ShellError::UnsupportedConfigValue(
                            "block or string".to_string(),
                            format!("{}", other.get_type()),
                            other.span()?,
                        ));
                    }
                }
            }
        }
        Value::Block {
            val: block_id,
            span: block_span,
            ..
        } => {
            output = run_hook_block(
                engine_state,
                stack,
                *block_id,
                input,
                arguments,
                *block_span,
            )?;
        }
        Value::Closure {
            val: block_id,
            span: block_span,
            ..
        } => {
            output = run_hook_block(
                engine_state,
                stack,
                *block_id,
                input,
                arguments,
                *block_span,
            )?;
        }
        other => {
            return Err(ShellError::UnsupportedConfigValue(
                "block, record, or list of records".into(),
                format!("{}", other.get_type()),
                other.span()?,
            ));
        }
    }

    let cwd = get_guaranteed_cwd(engine_state, stack);
    engine_state.merge_env(stack, cwd)?;

    Ok(output)
}

fn run_hook_block(
    engine_state: &EngineState,
    stack: &mut Stack,
    block_id: BlockId,
    optional_input: Option<PipelineData>,
    arguments: Vec<(String, Value)>,
    span: Span,
) -> Result<PipelineData, ShellError> {
    let block = engine_state.get_block(block_id);

    let input = optional_input.unwrap_or_else(PipelineData::empty);

    let mut callee_stack = stack.gather_captures(&block.captures);

    for (idx, PositionalArg { var_id, .. }) in
        block.signature.required_positional.iter().enumerate()
    {
        if let Some(var_id) = var_id {
            if let Some(arg) = arguments.get(idx) {
                callee_stack.add_var(*var_id, arg.1.clone())
            } else {
                return Err(ShellError::IncompatibleParametersSingle {
                    msg: "This hook block has too many parameters".into(),
                    span,
                });
            }
        }
    }

    let pipeline_data =
        eval_block_with_early_return(engine_state, &mut callee_stack, block, input, false, false)?;

    if let PipelineData::Value(Value::Error { error }, _) = pipeline_data {
        return Err(*error);
    }

    // If all went fine, preserve the environment of the called block
    let caller_env_vars = stack.get_env_var_names(engine_state);

    // remove env vars that are present in the caller but not in the callee
    // (the callee hid them)
    for var in caller_env_vars.iter() {
        if !callee_stack.has_env_var(engine_state, var) {
            stack.remove_env_var(engine_state, var);
        }
    }

    // add new env vars from callee to caller
    for (var, value) in callee_stack.get_stack_env_vars() {
        stack.add_env_var(var, value);
    }
    Ok(pipeline_data)
}
