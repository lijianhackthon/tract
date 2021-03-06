use std::str::FromStr;
#[allow(unused_imports)]
use tract_itertools::Itertools;

use tract_core::internal::*;
use tract_core::model::TypedModel;
use tract_hir::internal::*;
#[cfg(feature = "tf")]
use tract_tensorflow::tfpb::tensorflow::GraphDef;

use crate::display_params::DisplayParams;
use crate::errors::*;

use readings_probe::*;

use super::display_params;
use super::{info_usage, tensor};

use super::model::Model;

pub struct ModelError(pub Option<Box<dyn Model>>, pub CliError);

impl<E: Into<CliError>> From<E> for ModelError {
    fn from(e: E) -> ModelError {
        ModelError(None, e.into())
    }
}

#[derive(Debug)]
pub enum SomeGraphDef {
    NoGraphDef,
    #[cfg(feature = "kaldi")]
    Kaldi(tract_kaldi::KaldiProtoModel),
    #[cfg(feature = "onnx")]
    Onnx(tract_onnx::pb::ModelProto, tract_onnx::model::ParseResult),
    #[cfg(feature = "tf")]
    Tf(GraphDef),
}

/// Structure holding the parsed parameters.
pub struct Parameters {
    pub analyse_error: Option<TractError>,
    pub graph: SomeGraphDef,

    pub decluttered_model: Option<Arc<TypedModel>>,
    pub pulsed_model: Option<Arc<PulsedModel>>,

    pub tract_model: Arc<dyn Model>,

    #[cfg(feature = "conform")]
    pub tf_model: Option<tract_tensorflow::conform::tf::Tensorflow>,

    #[cfg(not(feature = "conform"))]
    #[allow(dead_code)]
    pub tf_model: (),

    pub input_values: Vec<Option<Arc<Tensor>>>,

    pub assertions: Assertions,

    pub machine_friendly: bool,
}

#[cfg(feature = "tf")]
type TfExt = tract_tensorflow::model::TfModelExtensions;
#[cfg(not(feature = "tf"))]
type TfExt = ();

impl Parameters {
    fn disco_model(matches: &clap::ArgMatches) -> CliResult<(std::path::PathBuf, bool)> {
        let filename = matches.value_of("model").ok_or("Model argument required")?;
        let filename = std::path::PathBuf::from(filename);
        let (filename, onnx_tc) = if !filename.exists() {
            bail!("model not found: {:?}", filename)
        } else if std::fs::metadata(&filename)?.is_dir() && filename.join("model.onnx").exists() {
            (filename.join("model.onnx"), true)
        } else {
            (filename, false)
        };
        Ok((filename, onnx_tc))
    }

    fn load_model(
        matches: &clap::ArgMatches,
        probe: Option<&Probe>,
        filename: &std::path::Path,
    ) -> CliResult<(SomeGraphDef, InferenceModel, Option<TfExt>)> {
        let need_graph =
            matches.is_present("proto") || matches.subcommand_name() == Some("compare-pbdir");

        let format = matches.value_of("format").unwrap_or(
            if filename.extension().and_then(|s| s.to_str()) == Some("onnx") {
                "onnx"
            } else {
                "tf"
            },
        );
        let triplet = match format {
            #[cfg(feature = "kaldi")]
            "kaldi" => {
                let kaldi = tract_kaldi::kaldi();
                info_usage("loaded framework (kaldi)", probe);
                let mut graph = kaldi.proto_model_for_path(&filename)?;
                info_usage("proto model loaded", probe);
                if let Some(i) = matches.value_of("kaldi_adjust_final_offset") {
                    graph.adjust_final_offset = i.parse()?;
                }
                let parsed = kaldi.model_for_proto_model(&graph)?;
                if need_graph {
                    (SomeGraphDef::Kaldi(graph), parsed, Option::<TfExt>::None)
                } else {
                    (SomeGraphDef::NoGraphDef, parsed, Option::<TfExt>::None)
                }
            }
            #[cfg(feature = "onnx")]
            "onnx" => {
                let onnx = tract_onnx::onnx();
                info_usage("loaded framework (onnx)", probe);
                let graph = onnx.proto_model_for_path(&filename)?;
                info_usage("proto model loaded", probe);
                let parsed = onnx.parse(&graph)?;
                if need_graph {
                    (SomeGraphDef::Onnx(graph, parsed.clone()), parsed.model, Option::<TfExt>::None)
                } else {
                    (SomeGraphDef::NoGraphDef, parsed.model, Option::<TfExt>::None)
                }
            }
            #[cfg(feature = "tf")]
            "tf" => {
                let tf = tract_tensorflow::tensorflow();
                info_usage("loaded framework (tf)", probe);
                let mut graph = tf.proto_model_for_path(&filename)?;
                info_usage("proto model loaded", probe);
                if matches.is_present("determinize") {
                    tract_tensorflow::Tensorflow::determinize(&mut graph)?;
                }
                let mut model_and_ext = tf.parse_graph(&graph)?;
                model_and_ext.1.initializing_nodes = matches
                    .values_of("tf_initializer_output_node")
                    .map(|values| {
                        values
                            .map(|name| model_and_ext.0.node_id_by_name(name))
                            .collect::<TractResult<Vec<usize>>>()
                    })
                    .transpose()?
                    .unwrap_or(vec![]);
                if need_graph {
                    (SomeGraphDef::Tf(graph), model_and_ext.0, Some(model_and_ext.1))
                } else {
                    (SomeGraphDef::NoGraphDef, model_and_ext.0, Some(model_and_ext.1))
                }
            }
            _ => bail!(
                "Format {} not supported. You may need to recompile tract with the right features.",
                format
            ),
        };
        Ok(triplet)
    }

    fn kaldi_downsample(raw_model: &mut InferenceModel, period: isize) -> CliResult<()> {
        if period != 1 {
            let mut outputs = raw_model.output_outlets()?.to_vec();
            let output_name = raw_model.node(outputs[0].node).name.clone();
            raw_model.node_mut(outputs[0].node).name = format!("{}-old", output_name);
            let id = raw_model.add_node(
                output_name,
                tract_core::ops::Downsample::new(0, period as _, 0),
                tvec!(InferenceFact::default()),
            )?;
            raw_model.add_edge(outputs[0], InletId::new(id, 0))?;
            outputs[0].node = id;
            raw_model.set_output_outlets(&*outputs)?;
        }
        Ok(())
    }

    fn kaldi_context(raw_model: &mut InferenceModel, left: usize, right: usize) -> CliResult<()> {
        let op = tract_core::ops::array::Pad::new(
            vec![(left, right), (0, 0)],
            tract_core::ops::array::PadMode::Edge,
        );
        let mut patch = InferenceModelPatch::default();
        for input in raw_model.input_outlets()? {
            let tap = patch.tap_model(&raw_model, *input)?;
            let pad = patch.wire_node(
                format!("{}-pad", raw_model.node(input.node).name),
                op.clone(),
                &[tap],
            )?[0];
            patch.shunt_outside(&raw_model, *input, pad)?;
        }
        patch.apply(raw_model)?;
        Ok(())
    }

    fn use_onnx_test_case_data_set(
        raw_model: &mut InferenceModel,
        input_values: &mut Vec<Option<Arc<Tensor>>>,
        assertions: &mut Assertions,
        inputs_dir: &std::path::Path,
    ) -> CliResult<()> {
        let files = inputs_dir
            .read_dir()?
            .map(|file| {
                let file = file?;
                let filename = file
                    .file_name()
                    .into_string()
                    .map_err(|s| format!("Can't convert OSString to String ({:?})", s))?;
                if filename.starts_with("input_") || filename.starts_with("output_") {
                    let ix = filename
                        .split("_")
                        .nth(1)
                        .unwrap()
                        .split(".")
                        .nth(0)
                        .unwrap()
                        .parse::<usize>()?;
                    let (name, tensor) = tensor::for_data(file.path().to_str().unwrap())?;
                    Ok(Some((ix, filename.starts_with("input_"), filename, name.unwrap(), tensor)))
                } else {
                    Ok(None)
                }
            })
            .collect::<CliResult<Vec<Option<_>>>>()?;
        let files = files.into_iter().filter_map(|x| x).collect::<Vec<_>>();
        let (inputs, outputs) = files.iter().partition::<Vec<_>, _>(|f| f.1);
        let inputs = inputs.into_iter().sorted_by_key(|f| f.0).collect::<Vec<_>>();
        let outputs = outputs.into_iter().sorted_by_key(|f| f.0).collect::<Vec<_>>();
        let input_names = inputs.iter().map(|i| &*i.3).collect::<Vec<_>>();
        let output_names = outputs.iter().map(|i| &*i.3).collect::<Vec<_>>();
        debug!("input_names from files: {:?}", input_names);
        debug!("output_names from files: {:?}", output_names);
        raw_model.set_input_names(input_names)?;
        raw_model.set_output_names(output_names)?;
        for (ix, _, filename, name, tensor) in inputs.into_iter() {
            debug!("Using {} as input {} ({}): {:?}", filename, ix, name, tensor);
            input_values[*ix] = tensor.value.concretize();
            raw_model.set_input_fact(*ix, tensor.clone().without_value())?;
        }
        let outputs = outputs
            .into_iter()
            .inspect(|(ix, _, filename, name, tensor)| {
                debug!("Using {} as output {} ({}): {:?}", filename, ix, name, tensor);
            })
            .map(|(_, _, _, _, tensor)| tensor.concretize())
            .collect();
        assertions.assert_outputs = Some(outputs);
        Ok(())
    }

    fn inputs(
        raw_model: &mut InferenceModel,
        assertions: &mut Assertions,
        matches: &clap::ArgMatches,
        filename: &std::path::Path,
        onnx_tc: bool,
    ) -> CliResult<Vec<Option<Arc<Tensor>>>> {
        let mut input_values = vec![None; raw_model.inputs.len()];

        if let Some(inputs) = matches.values_of("input") {
            for (ix, v) in inputs.enumerate() {
                let (name, t) = tensor::for_string(v)?;
                let outlet = if let Some(name) = name.filter(|s| s.len() > 0) {
                    let node = raw_model.node_by_name(&*name)?;
                    OutletId::new(node.id, 0)
                } else {
                    raw_model.input_outlets()?[ix]
                };
                input_values[ix] = t.value.concretize();
                if !raw_model.inputs.contains(&outlet) {
                    // shed edges from parents to us
                    for input in raw_model.node(outlet.node).inputs.clone() {
                        raw_model.node_mut(input.node).outputs[input.slot]
                            .successors
                            .retain(|s| s.node != outlet.node);
                    }
                    // clear our inputs and change ourselves to a source
                    raw_model.node_mut(outlet.node).inputs.clear();
                    raw_model.node_mut(outlet.node).op =
                        Box::new(tract_hir::ops::source::Source::new());
                }
                info!("Input #{}: {:?}", ix, t);
                raw_model.set_outlet_fact(outlet, t.without_value())?;
            }
        }

        if let Some(bundle) = matches.values_of("input_bundle") {
            for input in bundle {
                let mut npz = ndarray_npy::NpzReader::new(std::fs::File::open(input)?)?;
                for name in npz.names()? {
                    match tensor::for_npz(&mut npz, &*name) {
                        Ok(t) => debug!("{} contains {}: {:?}", input, name, t),
                        Err(r) => warn!("Could not read {} from {} ({})", name, input, r),
                    }
                }
                let input_outlets = raw_model.input_outlets()?.to_vec();
                for (ix, input) in input_outlets.iter().enumerate() {
                    let name = format!("{}.npy", raw_model.node(input.node).name);
                    if let Ok(t) = tensor::for_npz(&mut npz, &name) {
                        let shape = t.shape().to_vec();
                        let fact = InferenceFact::dt_shape(t.datum_type(), shape);
                        raw_model.set_input_fact(ix, fact.without_value())?;
                        input_values[ix] = Some(t.into_arc_tensor());
                    }
                }
            }
        }

        if onnx_tc {
            Self::use_onnx_test_case_data_set(
                raw_model,
                &mut input_values,
                assertions,
                filename.parent().unwrap().join("test_data_set_0").as_path(),
            )?
        }

        if let Some(tc) = matches.value_of("onnx_test_data_set") {
            Self::use_onnx_test_case_data_set(
                raw_model,
                &mut input_values,
                assertions,
                &std::path::Path::new(tc),
            )?
        }

        let const_inputs = matches.values_of("const_input").map(|c| c.collect()).unwrap_or(vec![]);
        for i in (0..raw_model.inputs.len()).rev() {
            let input = raw_model.inputs[i];
            if const_inputs.contains(&raw_model.node_name(input.node)) {
                if let Some(v) = input_values[i].take() {
                    raw_model.node_mut(input.node).op =
                        Box::new(tract_core::ops::konst::Const::new(v));
                } else {
                    bail!(
                        "Don't have value for input {}, can't make it const",
                        raw_model.node_name(input.node)
                    );
                }
                raw_model.inputs.remove(i);
            }
        }
        Ok(input_values)
    }

    fn pipeline(
        matches: &clap::ArgMatches,
        probe: Option<&readings_probe::Probe>,
        raw_model: InferenceModel,
        tf_model_extensions: Option<TfExt>,
    ) -> Result<(Arc<dyn Model>, Option<Arc<TypedModel>>, Option<Arc<PulsedModel>>), ModelError>
    {
        let keep_last = matches.is_present("verbose");
        let pulse: Option<usize> =
            matches.value_of("pulse").map(|s| s.parse::<usize>()).transpose()?;
        let concretize_stream_dim: Option<usize> =
            matches.value_of("concretize_stream_dim").map(|s| s.parse()).transpose()?;

        let mut inference_model: Option<Arc<InferenceModel>> = Some(Arc::new(raw_model));
        let mut typed_model: Option<Arc<TypedModel>> = None;
        let mut pulsed_model: Option<Arc<PulsedModel>> = None;

        let stop_at = matches.value_of("pass").unwrap_or(if matches.is_present("optimize") {
            "optimize"
        } else if concretize_stream_dim.is_some() {
            "concretize-stream-dim-declutter"
        } else if pulse.is_some() {
            "pulse-declutter"
        } else {
            "declutter"
        });
        info!("Will stop at {}", stop_at);

        macro_rules! stage {
            ($name:expr, $from:ident -> $to:ident, $block:expr) => {
                info!(concat!("Running '", $name, "'"));
                let last_model: Option<Box<dyn Model>> = if keep_last {
                    Some(Box::new((**($from.as_ref().unwrap())).clone()))
                } else {
                    None
                };
                let block: &dyn Fn(_) -> TractResult<_> = &$block;
                match block(Arc::try_unwrap($from.take().unwrap()).unwrap()) {
                    Ok(it) => {
                        $to = Some(Arc::new(it));
                    }
                    Err(e) => {
                        return Err(ModelError(last_model, e.into()));
                    }
                }
                if stop_at == $name {
                    return Ok(($to.clone().unwrap(), typed_model, pulsed_model));
                }
                info_usage(concat!("after ", $name), probe);
            };
        };

        stage!("load", inference_model -> inference_model, |m:InferenceModel| TractResult::Ok(m));
        stage!("analyse", inference_model -> inference_model, 
               |mut m:InferenceModel| { m.analyse(matches.is_present("analyse_fail_fast"))?; TractResult::Ok(m) });
        if let Some(ext) = tf_model_extensions {
            #[cfg(feature = "tf")]
            stage!("tf-preproc", inference_model -> inference_model, |m:InferenceModel| ext.preproc(m));
        }
        stage!("incorporate", inference_model -> inference_model, |m:InferenceModel| { m.incorporate()});
        stage!("type", inference_model -> typed_model, |m:InferenceModel| m.into_typed());
        stage!("declutter", typed_model -> typed_model, |m:TypedModel| m.declutter());
        if let Some(dim) = concretize_stream_dim {
            stage!("concretize-stream-dim", typed_model -> typed_model, |m:TypedModel| m.concretize_stream_dim(dim) );
            stage!("concretize-stream-dim-declutter", typed_model -> typed_model, |m:TypedModel| m.declutter());
        } else if let Some(pulse) = pulse {
            stage!("pulse", typed_model -> pulsed_model, |m:TypedModel| ::tract_core::pulse::PulsedModel::new(&m, pulse));
            stage!("pulse-to-type", pulsed_model -> typed_model, |m:PulsedModel| m.into_typed());
            stage!("pulse-declutter", typed_model -> typed_model, |m:TypedModel| m.declutter());
        }
        info_usage("before optimize", probe);
        stage!("optimize", typed_model -> typed_model, |m:TypedModel| m.optimize());
        Ok((typed_model.clone().unwrap(), typed_model, pulsed_model))
    }

    #[allow(unused_variables)]
    /// Parses the command-line arguments.
    pub fn from_clap(
        matches: &clap::ArgMatches,
        probe: Option<&Probe>,
    ) -> Result<Parameters, ModelError> {
        let (filename, onnx_tc) = Self::disco_model(matches)?;
        let (mut graph, mut raw_model, tf_model_extensions) =
            Self::load_model(matches, probe, &filename)?;

        info!("Model {:?} loaded", filename);
        info_usage("model loaded", probe);

        let need_tensorflow_model = matches.subcommand_name() == Some("compare");

        #[cfg(not(feature = "conform"))]
        let tf_model = ();
        #[cfg(feature = "conform")]
        let tf_model = if format == "tf" && need_tensorflow_model {
            info!("Tensorflow version: {}", tract_tensorflow::conform::tf::version());
            if matches.is_present("determinize") {
                if let SomeGraphDef::Tf(ref graph) = graph {
                    let graph = graph.write_to_bytes().unwrap();
                    Some(tract_tensorflow::conform::tf::for_slice(&graph)?)
                } else {
                    unreachable!()
                }
            } else {
                Some(tract_tensorflow::conform::tf::for_path(&filename)?)
            }
        } else {
            None
        };

        if !matches.is_present("proto") && matches.subcommand_name() != Some("compare-pbdir") {
            graph = SomeGraphDef::NoGraphDef;
        }

        if let Some(inputs) = matches.values_of("input") {
            let names = inputs
                .map(|t| Ok(tensor::for_string(t)?.0))
                .collect::<CliResult<Vec<Option<String>>>>()?;
            if names.iter().all(|s| s.is_some() && s.as_ref().unwrap().len() > 0) {
                let names: Vec<String> = names.into_iter().map(|s| s.unwrap()).collect();
                raw_model.set_input_names(names)?;
            }
        }

        if let Some(inputs) = matches.values_of("input_node") {
            raw_model.set_input_names(inputs)?;
        };

        if let Some(outputs) = matches.values_of("output_node") {
            raw_model.set_output_names(outputs)?;
        };

        if let Some(override_facts) = matches.values_of("override_fact") {
            for fact in override_facts {
                let (name, fact) = tensor::for_string(fact)?;
                let node = raw_model.node_by_name(name.unwrap())?.id;
                raw_model.set_outlet_fact(OutletId::new(node, 0), fact)?;
            }
        };

        let output_names: Vec<String> = raw_model
            .output_outlets()?
            .iter()
            .map(|o| raw_model.node(o.node).name.to_string())
            .collect();

        let mut assertions = Assertions::from_clap(matches, &output_names)?;

        if let Some(sub) = matches.value_of("kaldi_downsample") {
            Self::kaldi_downsample(&mut raw_model, sub.parse()?)?;
        }

        if matches.value_of("kaldi_left_context").is_some()
            || matches.value_of("kaldi_right_context").is_some()
        {
            let left = matches.value_of("kaldi_left_context").unwrap_or("0").parse()?;
            let right = matches.value_of("kaldi_right_context").unwrap_or("0").parse()?;
            Self::kaldi_context(&mut raw_model, left, right)?;
        }

        let input_values =
            Self::inputs(&mut raw_model, &mut assertions, matches, &filename, onnx_tc)?;

        if matches.is_present("partial") {
            raw_model = raw_model.eliminate_dead_branches()?;
        }

        Self::pipeline(matches, probe, raw_model, tf_model_extensions).map(
            |(tract_model, decluttered_model, pulsed_model)| {
                info!("Model ready");
                info_usage("model ready", probe);
                Parameters {
                    analyse_error: None,
                    graph,
                    decluttered_model,
                    pulsed_model,
                    tract_model,
                    tf_model,
                    input_values,
                    assertions,
                    machine_friendly: matches.is_present("machine_friendly"),
                }
            },
        )
    }
}

pub struct BenchLimits {
    pub max_iters: usize,
    pub max_time: std::time::Duration,
}

impl BenchLimits {
    pub fn from_clap(matches: &clap::ArgMatches) -> CliResult<BenchLimits> {
        let max_iters =
            matches.value_of("max_iters").map(usize::from_str).transpose()?.unwrap_or(100_000);
        let max_time = matches
            .value_of("max-time")
            .map(u64::from_str)
            .transpose()?
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_secs(5));
        Ok(BenchLimits { max_iters, max_time })
    }
}

pub fn display_params_from_clap(
    root_matches: &clap::ArgMatches,
    matches: &clap::ArgMatches,
) -> CliResult<DisplayParams> {
    Ok(DisplayParams {
        konst: matches.is_present("const"),
        cost: matches.is_present("cost"),
        profile: matches.is_present("profile"),
        left_column_width: 0,
        invariants: matches.is_present("invariants"),
        quiet: matches.is_present("quiet"),
        natural_order: matches.is_present("natural-order"),
        debug_op: matches.is_present("debug-op"),
        node_ids: matches.values_of("node_id").map(|values| {
            values.map(|id| tvec!((id.parse::<usize>().unwrap(), "".to_string()))).collect()
        }),
        node_name: matches.value_of("node_name").map(String::from),
        op_name: matches.value_of("op_name").map(String::from),
        //        successors: matches.value_of("successors").map(|id| id.parse().unwrap()),
        expect_canonic: root_matches.value_of("pass").unwrap_or("declutter") == "declutter"
            && !root_matches.is_present("optimize"),
        outlet_labels: matches.is_present("outlet-labels"),
        io: if matches.is_present("io-long") {
            display_params::Io::Long
        } else if matches.is_present("io-none") {
            display_params::Io::None
        } else {
            display_params::Io::Short
        },
        info: matches.is_present("info"),
        json: matches.is_present("json"),
    })
}

#[derive(Debug)]
pub struct Assertions {
    pub assert_outputs: Option<Vec<Option<Arc<Tensor>>>>,
    pub assert_output_facts: Option<Vec<InferenceFact>>,
}

impl Assertions {
    fn from_clap(sub_matches: &clap::ArgMatches, output_names: &[String]) -> CliResult<Assertions> {
        let mut assert_outputs: Option<Vec<Option<Arc<Tensor>>>> = sub_matches
            .values_of("assert-output")
            .map(|vs| vs.map(|v| tensor::for_string(v).unwrap().1.value.concretize()).collect());

        if assert_outputs.is_none() {
            if sub_matches.values_of("assert-output-bundle").is_some() {
                let values = output_names
                    .iter()
                    .map(move |name| {
                        let npy_name = format!("{}.npy", name);
                        for output_bundle in sub_matches.values_of("assert-output-bundle").unwrap()
                        {
                            let mut npz =
                                ndarray_npy::NpzReader::new(std::fs::File::open(output_bundle)?)?;
                            if let Ok(t) = tensor::for_npz(&mut npz, &npy_name) {
                                return Ok(Some(t.into_arc_tensor()));
                            }
                        }
                        return Ok(None);
                    })
                    .collect::<CliResult<_>>()?;
                assert_outputs = Some(values)
            }
        }

        let assert_output_facts: Option<Vec<InferenceFact>> = sub_matches
            .values_of("assert-output-fact")
            .map(|vs| vs.map(|v| tensor::for_string(v).unwrap().1).collect());
        Ok(Assertions { assert_outputs, assert_output_facts })
    }
}
