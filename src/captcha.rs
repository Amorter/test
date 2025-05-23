use crate::environment::CaptchaEnvironment;
use crate::model::Model;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba};
use ndarray::{Array2, Array4, ArrayView2, Axis, Dim, Ix2, s};
use ort::inputs;
use ort::session::Session;
use std::error::Error;
use std::sync::Arc;
use crate::lapjv;

pub trait CaptchaBreaker {
    fn build(captcha_environment: &CaptchaEnvironment) -> Result<Self, Box<dyn Error>>
    where
        Self: Sized;
}

#[cfg(feature = "chinese_click_0")]
#[derive(Debug)]
pub struct ChineseClick0 {
    yolo11n: Arc<Session>,
    siamese: Arc<Session>,
}

impl CaptchaBreaker for ChineseClick0 {
    fn build(captcha_environment: &CaptchaEnvironment) -> Result<Self, Box<dyn Error>> {
        let session = captcha_environment.load_models(vec![Model::Yolo11n, Model::Siamese])?;
        Ok(ChineseClick0 {
            yolo11n: session[0].clone(),
            siamese: session[1].clone(),
        })
    }
}


#[derive(Debug)]
struct Bbox {
    x_min: f32,
    y_min: f32,
    x_max: f32,
    y_max: f32,
    confidence: f32,
    class: f32,
}

impl ChineseClick0 {

    pub fn run(&self, image: &DynamicImage) -> Result<Vec<(f32, f32)>, Box<dyn Error>> {
        // 1. 图像预处理
        let processed_image = self.preprocess_image(&image);
        // 2. YOLO目标检测
        let bboxes = self.detect_objects(&processed_image)?;
        // 3. 分离答案框和问题框
        let (ans_boxes, question_boxes) = self.split_boxes(bboxes);
        // 4. 截取并预处理图像块
        let combined_images = self.crop_and_resize(&processed_image, &ans_boxes, &question_boxes);
        // 5. 特征提取
        let features = self.extract_features(&combined_images)?;
        // 6. 构建匹配矩阵并计算匹配
        let matches = self.match_features(&features, ans_boxes.len())?;
        // 7. 生成结果
        Ok(self.generate_results(&ans_boxes, &matches))
    }

    /// 图像预处理
    fn preprocess_image(&self, image: &DynamicImage) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
        let (width, height) = (image.width(), image.height());
        assert!(width <= 384 && height <= 384, "不能输入大于384长宽的图片!");

        let mut new_image = ImageBuffer::from_pixel(384, 384, Rgba([0u8, 0u8, 0u8, 255u8]));
        for y in 0..height {
            for x in 0..width {
                let pixel = image.get_pixel(x, y);
                new_image.put_pixel(x, y, pixel);
            }
        }
        new_image
    }

    /// 目标检测
    fn detect_objects(&self, image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Result<Vec<Bbox> , Box<dyn Error>>{
        let mut input = Array4::<f32>::zeros((1, 3, 384, 384));
        for x in 0..384 {
            for y in 0..384 {
                let pixel = image.get_pixel(x, y);
                input[[0, 0, y as usize, x as usize]] = pixel[0] as f32 / 255.0;
                input[[0, 1, y as usize, x as usize]] = pixel[1] as f32 / 255.0;
                input[[0, 2, y as usize, x as usize]] = pixel[2] as f32 / 255.0;
            }
        }

        let outputs = self
            .yolo11n
            .run(inputs!["images" => input]?)?;
        let output = outputs["output0"]
            .try_extract_tensor::<f32>()?
            .slice_move(s![0, .., ..]);

        Ok(output
            .axis_iter(Axis(0))
            .filter(|row| row[Dim(4)] > 0.5)
            .map(|row| Bbox {
                x_min: row[Dim(0)],
                y_min: row[Dim(1)],
                x_max: row[Dim(2)],
                y_max: row[Dim(3)],
                confidence: row[Dim(4)],
                class: row[Dim(5)],
            })
            .collect())
    }

    /// 分离答案框和问题框
    fn split_boxes(&self, mut bboxes: Vec<Bbox>) -> (Vec<Bbox>, Vec<Bbox>) {
        bboxes.sort_by_key(|b| (b.x_min*100f32) as u32);
        bboxes.drain(..).partition(|b| b.y_min < 344.0)
    }

    /// 截取并预处理图像块
    fn crop_and_resize(
        &self,
        image: &ImageBuffer<Rgba<u8>, Vec<u8>>,
        ans_boxes: &[Bbox],
        question_boxes: &[Bbox],
    ) -> Array4<f32> {
        const TARGET_SIZE: u32 = 96;
        let batch_size = ans_boxes.len() + question_boxes.len();
        let mut batch = Array4::zeros((batch_size, 3, TARGET_SIZE as usize, TARGET_SIZE as usize));

        // 处理答案框
        self.process_boxes(&mut batch, image, ans_boxes, 0..ans_boxes.len());

        // 处理问题框
        self.process_boxes(
            &mut batch,
            image,
            question_boxes,
            ans_boxes.len()..batch_size,
        );

        batch
    }

    /// 处理图像块
    fn process_boxes(
        &self,
        batch: &mut Array4<f32>,
        image: &ImageBuffer<Rgba<u8>, Vec<u8>>,
        boxes: &[Bbox],
        indices: std::ops::Range<usize>,
    ) {
        const TARGET_SIZE: u32 = 96;
        for (i, bbox) in boxes.iter().enumerate() {
            let index = indices.start + i;
            let cropped = image
                .view(
                    bbox.x_min as u32,
                    bbox.y_min as u32,
                    (bbox.x_max - bbox.x_min) as u32,
                    (bbox.y_max - bbox.y_min) as u32,
                )
                .to_image();
            let resized = image::imageops::resize(
                &cropped,
                TARGET_SIZE,
                TARGET_SIZE,
                image::imageops::FilterType::Lanczos3,
            );

            for y in 0..TARGET_SIZE {
                for x in 0..TARGET_SIZE {
                    let pixel = resized.get_pixel(x, y);
                    batch[[index, 0, y as usize, x as usize]] = pixel[0] as f32 / 255.0;
                    batch[[index, 1, y as usize, x as usize]] = pixel[1] as f32 / 255.0;
                    batch[[index, 2, y as usize, x as usize]] = pixel[2] as f32 / 255.0;
                }
            }
        }
    }

    /// 特征提取
    fn extract_features(&self, images: &Array4<f32>) -> Result<Array2<f32>, Box<dyn Error >> {
        let outputs = self
            .siamese
            .run(inputs!["input" => images.clone()]?)?;
        Ok(outputs["output"]
            .try_extract_tensor::<f32>()?
            .into_dimensionality::<Ix2>()?
            .to_owned())
    }

    /// 构建匹配矩阵并计算匹配
    fn match_features(&self, features: &Array2<f32>, ans_count: usize) -> Result<Vec<usize>, Box<dyn Error>> {
        // 分离特征
        let (ans_features, question_features) = features.view().split_at(Axis(0), ans_count);

        // 构建成本矩阵
        let cost_matrix = self.build_cost_matrix(&question_features, &ans_features);

        // 匈牙利算法
        Ok(self.hungarian(&cost_matrix)?.0)
    }

    /// 构建成本矩阵
    fn build_cost_matrix(&self, question: &ArrayView2<f32>, ans: &ArrayView2<f32>) -> Array2<f32> {
        let mut matrix = Array2::zeros((question.nrows(), ans.nrows()));
        for (i, q_feat) in question.rows().into_iter().enumerate() {
            for (j, a_feat) in ans.rows().into_iter().enumerate() {
                matrix[[i, j]] = (q_feat.to_owned() - a_feat.to_owned())
                    .mapv(|x| x.powi(2))
                    .sum()
                    .sqrt();
            }
        }
        matrix
    }

    /// 匈牙利算法
    fn hungarian(&self, matrix: &Array2<f32>) -> Result<(Vec<usize>, Vec<usize>), Box<dyn Error>> {
        Ok(lapjv::lapjv(matrix)?)
    }

    /// 生成结果字符串
    fn generate_results(&self, ans_boxes: &[Bbox], indices: &[usize]) -> Vec<(f32, f32)> {
        indices
            .iter()
            .map(|&i| {
                let b = &ans_boxes[i];
                ((b.x_min + b.x_max) / 2.0, (b.y_min + b.y_max) / 2.0)
            })
            .collect()
    }
}
