use crate::ai::utils;
use candle_core::{DType, Tensor, D};
use candle_datasets;
use candle_nn::{loss, ops, Conv2d, Linear, ModuleT, Optimizer, VarBuilder, VarMap};
use rand::prelude::*;

const LABELS: usize = 10;

#[derive(Debug)]
pub struct ConvNet {
    conv1: Conv2d,
    conv2: Conv2d,
    conv3: Conv2d,
    fc1: Linear,
    fc2: Linear,
    dropout: candle_nn::Dropout,
}

impl ConvNet {
    pub fn new(vm: &mut VarMap) -> candle_core::Result<Self> {
        // Get Device
        let dev = candle_core::Device::cuda_if_available(0)?;

        // Create vb
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &dev);

        let conv1 = candle_nn::conv2d(1, 64, 2, Default::default(), vb.pp("c1"))?;
        let conv2 = candle_nn::conv2d(64, 64, 2, Default::default(), vb.pp("c2"))?;
        let conv3 = candle_nn::conv2d(64, 64, 2, Default::default(), vb.pp("c3"))?;
        let fc1 = candle_nn::linear(1600, 64, vb.pp("fc1"))?;
        let fc2 = candle_nn::linear(64, LABELS, vb.pp("fc2"))?;
        let dropout = candle_nn::Dropout::new(0.25);
        Ok(Self {
            conv1,
            conv2,
            conv3,
            fc1,
            fc2,
            dropout,
        })
    }

    pub fn new_from_file(vm: &mut VarMap, path: &str) -> candle_core::Result<Self> {
        // Create a new ConvNet
        let model = ConvNet::new(vm).expect("Failed to create model");

        // Load weights and biases from file
        vm.load(path)
            .expect("Failed to load weights and biases from file");

        // Return the model
        return Ok(model);
    }

    fn forward(&self, xs: &Tensor, train: bool) -> candle_core::Result<Tensor> {
        let mut xs = xs.reshape(&[xs.dim(0)?, 1, 28, 28])?;
        xs = xs.apply(&self.conv1)?.relu()?;
        xs = xs.max_pool2d(2)?;
        xs = self.dropout.forward_t(&xs, train)?;

        xs = xs.apply(&self.conv2)?.relu()?;
        xs = xs.max_pool2d(2)?;
        xs = self.dropout.forward_t(&xs, train)?;

        xs = xs.apply(&self.conv3)?.relu()?;
        xs = self.dropout.forward_t(&xs, train)?;

        xs = xs.flatten_from(1)?;

        xs = xs.apply(&self.fc1)?.relu()?;
        xs = xs.apply(&self.fc2)?;
        Ok(xs)
    }

    pub fn predict(
        &self,
        image: &Tensor,
        device: &candle_core::Device,
    ) -> candle_core::Result<u32> {
        // Forward pass through the model
        let logits = self.forward(&image.to_device(device)?, false)?;
        let label = logits.argmax(D::Minus1)?;
        let label = label.squeeze(0)?;
        let label = label.to_scalar::<u32>()?;
        Ok(label)
    }

    pub fn train(
        &self,
        m: &candle_datasets::vision::Dataset,
        args: &TrainingArgs,
        varmap: &mut VarMap,
    ) -> candle_core::Result<()> {
        let dev = candle_core::Device::cuda_if_available(0)?;

        let train_labels = m.train_labels.to_dtype(DType::U32)?.to_device(&dev)?;
        let train_images = m.train_images.to_device(&dev)?;

        if let Some(load) = &args.load {
            println!("loading weights from {load}");
            varmap.load(load)?
        }

        let adamw_params = candle_nn::ParamsAdamW {
            lr: args.learning_rate,
            ..Default::default()
        };

        let mut opt = candle_nn::AdamW::new(varmap.all_vars(), adamw_params)?;

        let test_images = m.test_images.to_device(&dev)?;
        let test_labels = m.test_labels.to_dtype(DType::U32)?.to_device(&dev)?;

        let n_batches = train_images.dim(0)? / args.batch_size;

        let mut batch_idxs = (0..n_batches).collect::<Vec<usize>>();

        for epoch in 1..=args.epochs {
            let mut sum_loss = 0f32;

            batch_idxs.shuffle(&mut thread_rng());

            for batch_idx in batch_idxs.iter() {
                let train_images =
                    train_images.narrow(0, batch_idx * args.batch_size, args.batch_size)?;
                let train_labels =
                    train_labels.narrow(0, batch_idx * args.batch_size, args.batch_size)?;
                let logits = self.forward(&train_images, true)?;
                let log_sm = ops::log_softmax(&logits, D::Minus1)?;
                let loss = loss::nll(&log_sm, &train_labels)?;
                opt.backward_step(&loss)?;
                sum_loss += loss.to_vec0::<f32>()?;
            }

            let avg_loss = sum_loss / n_batches as f32;

            let test_logits = self.forward(&test_images, false)?;
            let test_prediction = test_logits.argmax(D::Minus1)?;

            let sum_ok = test_prediction
                .eq(&test_labels)?
                .to_dtype(DType::F32)?
                .sum_all()?
                .to_scalar::<f32>()?;

            let test_accuracy = sum_ok / test_labels.dims1()? as f32;

            println!(
                "{epoch:4} train loss {:8.5} test acc: {:5.2}%",
                avg_loss,
                100. * test_accuracy
            );
        }
        if let Some(save) = &args.save {
            println!("saving trained weights in {save}");
            varmap.save(save)?
        }

        Ok(())
    }

    pub fn test(
        &self,
        device: &candle_core::Device,
        data: &candle_datasets::vision::Dataset,
        batch_size: usize,
    ) -> candle_core::Result<()> {
        let test_start_time = std::time::Instant::now();

        let test_images = data.test_images.to_device(device)?;
        let test_labels = data.test_labels.to_device(device)?;
        let n_batches = test_images.dim(0)? / batch_size;
        let mut correct_predictions = 0;
        let mut total_predictions = 0;

        for i in 0..n_batches {
            let images = test_images.narrow(0, i * batch_size, batch_size)?;
            let labels = test_labels.narrow(0, i * batch_size, batch_size)?;

            let labels = labels.to_dtype(DType::U32)?;

            let logits = self.forward(&images, false)?;

            let predictions = logits.argmax(D::Minus1)?;

            correct_predictions += predictions
                .eq(&labels)?
                .to_dtype(DType::F32)?
                .sum_all()?
                .to_scalar::<f32>()? as usize;

            total_predictions += batch_size;
        }

        let accuracy = correct_predictions as f32 / total_predictions as f32 * 100.0;

        println!(
            "Test Accuracy: {:.2}% - took {:.2} seconds",
            accuracy,
            test_start_time.elapsed().as_secs_f64()
        );

        Ok(())
    }
}

pub struct TrainingArgs {
    pub epochs: usize,
    pub learning_rate: f64,
    pub batch_size: usize,
    pub load: Option<String>,
    pub save: Option<String>,
}
