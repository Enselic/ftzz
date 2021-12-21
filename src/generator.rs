use std::{
    cmp::{max, min},
    collections::VecDeque,
    fs::create_dir_all,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context};
use derive_builder::Builder;
use log::{debug, info};
use num_format::{SystemLocale, ToFormattedString};
use rand::{distributions::Distribution, RngCore, SeedableRng};
use rand_distr::{LogNormal, Normal};
use rand_xorshift::XorShiftRng;
use tokio::task;

use crate::errors::{CliExitAnyhowWrapper, CliResult};

#[derive(Builder, Debug)]
#[builder(build_fn(validate = "Self::validate"))]
pub struct Generator {
    root_dir: PathBuf,
    num_files: usize,
    #[builder(default = "5")]
    max_depth: u32,
    #[builder(default = "self.default_ftd_ratio()")]
    file_to_dir_ratio: usize,
    #[builder(default = "0")]
    entropy: u64,
}

impl GeneratorBuilder {
    fn validate(&self) -> Result<(), String> {
        // TODO use if let chains once that feature is stabilized
        if let Some(n) = self.num_files {
            if n < 1 {
                return Err("num_files must be strictly positive".to_string());
            }
        }
        if let Some(ratio) = self.file_to_dir_ratio {
            if ratio < 1 {
                return Err("file_to_dir_ratio must be strictly positive".to_string());
            }

            if let Some(num_files) = self.num_files {
                if ratio > num_files {
                    return Err(format!(
                        "The file to dir ratio ({}) cannot be larger than the number of files to generate ({}).",
                        ratio,
                        num_files,
                    ));
                }
            }
        }

        Ok(())
    }

    fn default_ftd_ratio(&self) -> usize {
        max(self.num_files.unwrap() / 1000, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_params_succeeds() {
        let g = GeneratorBuilder::default()
            .root_dir(PathBuf::from("abc"))
            .num_files(1)
            .build()
            .unwrap();

        assert_eq!(g.root_dir, PathBuf::from("abc"));
        assert_eq!(g.num_files, 1);
        assert_eq!(g.max_depth, 5);
        assert_eq!(g.file_to_dir_ratio, 1);
        assert_eq!(g.entropy, 0);
    }

    #[test]
    fn zero_files_fails() {
        let g = GeneratorBuilder::default()
            .root_dir(PathBuf::from("abc"))
            .num_files(0)
            .build();

        assert!(g.is_err());
    }

    #[test]
    fn zero_ratio_fails() {
        let g = GeneratorBuilder::default()
            .root_dir(PathBuf::from("abc"))
            .num_files(1)
            .file_to_dir_ratio(0)
            .build();

        assert!(g.is_err());
    }

    #[test]
    fn ratio_greater_than_num_files_fails() {
        let g = GeneratorBuilder::default()
            .root_dir(PathBuf::from("abc"))
            .num_files(1)
            .file_to_dir_ratio(2)
            .build();

        assert!(g.is_err());
    }
}

impl Generator {
    pub fn generate(self) -> CliResult<()> {
        let options = validated_options(self)?;
        print_configuration_info(&options);
        print_stats(run_generator(options)?);
        Ok(())
    }
}

#[derive(Debug)]
struct Configuration {
    root_dir: PathBuf,
    files: usize,
    files_per_dir: f64,
    dirs_per_dir: f64,
    max_depth: u32,
    entropy: u64,

    informational_dirs_per_dir: usize,
    informational_total_dirs: usize,
}

#[derive(Debug)]
struct GeneratorStats {
    files: usize,
    dirs: usize,
}

fn validated_options(generator: Generator) -> CliResult<Configuration> {
    create_dir_all(&generator.root_dir)
        .with_context(|| format!("Failed to create directory {:?}", generator.root_dir))
        .with_code(exitcode::IOERR)?;
    if generator
        .root_dir
        .read_dir()
        .with_context(|| format!("Failed to read directory {:?}", generator.root_dir))
        .with_code(exitcode::IOERR)?
        .count()
        != 0
    {
        return Err(anyhow!(format!(
            "The root directory {:?} must be empty.",
            generator.root_dir,
        )))
        .with_code(exitcode::DATAERR);
    }

    if generator.max_depth == 0 {
        return Ok(Configuration {
            root_dir: generator.root_dir,
            files: generator.num_files,
            files_per_dir: generator.num_files as f64,
            dirs_per_dir: 0.,
            max_depth: 0,
            entropy: generator.entropy,

            informational_dirs_per_dir: 0,
            informational_total_dirs: 1,
        });
    }

    let ratio = generator.file_to_dir_ratio as f64;
    let num_dirs = generator.num_files as f64 / ratio;
    // This formula was derived from the following equation:
    // num_dirs = unknown_num_dirs_per_dir^max_depth
    let dirs_per_dir = 2f64.powf(num_dirs.log2() / generator.max_depth as f64);

    Ok(Configuration {
        root_dir: generator.root_dir,
        files: generator.num_files,
        files_per_dir: ratio,
        dirs_per_dir,
        max_depth: generator.max_depth,
        entropy: generator.entropy,

        informational_dirs_per_dir: dirs_per_dir.round() as usize,
        informational_total_dirs: num_dirs.round() as usize,
    })
}

fn print_configuration_info(config: &Configuration) {
    let locale = SystemLocale::new().unwrap();
    println!(
        "About {} {files_maybe_plural} will be generated in approximately \
        {} {directories_maybe_plural} distributed across a tree of maximum depth {} where each \
        directory contains approximately {} other {dpd_directories_maybe_plural}.",
        config.files.to_formatted_string(&locale),
        config.informational_total_dirs.to_formatted_string(&locale),
        config.max_depth.to_formatted_string(&locale),
        config
            .informational_dirs_per_dir
            .to_formatted_string(&locale),
        files_maybe_plural = if config.files == 1 { "file" } else { "files" },
        directories_maybe_plural = if config.informational_total_dirs == 1 {
            "directory"
        } else {
            "directories"
        },
        dpd_directories_maybe_plural = if config.informational_dirs_per_dir == 1 {
            "directory"
        } else {
            "directories"
        },
    );
}

fn print_stats(stats: GeneratorStats) {
    let locale = SystemLocale::new().unwrap();
    println!(
        "Created {} {files_maybe_plural} across {} {directories_maybe_plural}.",
        stats.files.to_formatted_string(&locale),
        stats.dirs.to_formatted_string(&locale),
        files_maybe_plural = if stats.files == 1 { "file" } else { "files" },
        directories_maybe_plural = if stats.dirs == 1 {
            "directory"
        } else {
            "directories"
        },
    );
}

/// Specialized cache for file names that takes advantage of our monotonically increasing integer
/// naming scheme.
///
/// We intentionally use a thread-*un*safe raw fixed-size buffer to eliminate an Arc.
#[derive(Debug, Copy, Clone)]
struct FileNameCache {
    file_cache: (*mut String, usize),
    dir_cache: (*mut String, usize),
}

unsafe impl Send for FileNameCache {}

impl FileNameCache {
    fn alloc(config: &Configuration) -> Self {
        let num_cache_entries = config.files_per_dir + config.dirs_per_dir;
        let files_percentage = config.files_per_dir / num_cache_entries;

        // Overestimate since the cache can't grow
        let num_cache_entries = 1.5 * num_cache_entries;
        // Max out the cache size at 1MiB
        let num_cache_entries = f64::min((1 << 20) as f64, num_cache_entries);

        let file_entries = files_percentage * num_cache_entries;
        let dir_entries = num_cache_entries - file_entries;

        let mut file_cache =
            ManuallyDrop::new(Vec::<String>::with_capacity(file_entries.round() as usize));
        let mut dir_cache =
            ManuallyDrop::new(Vec::<String>::with_capacity(dir_entries.round() as usize));

        debug!("Allocated {} file cache entries.", file_cache.capacity());
        debug!(
            "Allocated {} directory cache entries.",
            dir_cache.capacity()
        );

        unsafe {
            let cap = file_cache.capacity();
            file_cache.set_len(cap);
            let cap = dir_cache.capacity();
            dir_cache.set_len(cap);

            for i in 0..file_cache.len() {
                file_cache
                    .as_mut_ptr()
                    .add(i)
                    .write(FileNameCache::file_name(i));
            }
            for i in 0..dir_cache.len() {
                dir_cache
                    .as_mut_ptr()
                    .add(i)
                    .write(FileNameCache::dir_name(i));
            }
        }

        FileNameCache {
            file_cache: (file_cache.as_mut_ptr(), file_cache.len()),
            dir_cache: (dir_cache.as_mut_ptr(), dir_cache.len()),
        }
    }

    fn free(self) {
        unsafe {
            Vec::from_raw_parts(self.file_cache.0, self.file_cache.1, self.file_cache.1);
            Vec::from_raw_parts(self.dir_cache.0, self.dir_cache.1, self.dir_cache.1);
        }
    }

    fn push_file_name(self, i: usize, path: &mut PathBuf) {
        if i >= self.file_cache.1 {
            path.push(FileNameCache::file_name(i));
            return;
        }

        let file_cache = ManuallyDrop::new(unsafe {
            Vec::from_raw_parts(self.file_cache.0, self.file_cache.1, self.file_cache.1)
        });
        path.push(&file_cache[i]);
    }

    fn push_dir_name(self, i: usize, path: &mut PathBuf) {
        if i >= self.dir_cache.1 {
            path.push(FileNameCache::dir_name(i));
            return;
        }

        let dir_cache = ManuallyDrop::new(unsafe {
            Vec::from_raw_parts(self.dir_cache.0, self.dir_cache.1, self.dir_cache.1)
        });
        path.push(&dir_cache[i]);
    }

    #[inline]
    fn file_name(i: usize) -> String {
        i.to_string()
    }

    #[inline]
    fn dir_name(i: usize) -> String {
        format!("{}.dir", i)
    }
}

fn run_generator(config: Configuration) -> CliResult<GeneratorStats> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(num_cpus::get())
        .build()
        .with_context(|| "Failed to create tokio runtime")
        .with_code(exitcode::OSERR)?;

    info!("Starting config: {:?}", config);
    runtime.block_on(run_generator_async(config))
}

async fn run_generator_async(config: Configuration) -> CliResult<GeneratorStats> {
    let cache = FileNameCache::alloc(&config);
    let mut random = {
        let seed = ((config.files.wrapping_add(config.max_depth as usize) as f64
            * (config.files_per_dir + config.dirs_per_dir)) as u64)
            .wrapping_add(config.entropy);
        debug!("Starting seed: {}", seed);
        XorShiftRng::seed_from_u64(seed)
    };
    let mut stack = VecDeque::with_capacity(config.max_depth as usize);
    let mut tasks = Vec::with_capacity(min(1 << 23, config.files * 3 / 2));
    let mut target_dir = config.root_dir;
    let mut stats = GeneratorStats { files: 0, dirs: 0 };

    debug!("Allocated {} task entries.", tasks.capacity());

    'outer: loop {
        let num_files_to_generate = config.files_per_dir.num_to_generate(&mut random);
        let num_dirs_to_generate = if stack.len() < config.max_depth as usize {
            config.dirs_per_dir.num_to_generate(&mut random)
        } else {
            0
        };

        {
            stats.files += num_files_to_generate;
            stats.dirs += num_dirs_to_generate;

            let target_dir = target_dir.clone();
            tasks.push(task::spawn_blocking(move || {
                create_files_and_dirs(
                    target_dir,
                    num_files_to_generate,
                    num_dirs_to_generate,
                    cache,
                )
            }));
        }

        if num_dirs_to_generate > 0 {
            stack.push_front((num_dirs_to_generate - 1, num_dirs_to_generate));
            cache.push_dir_name(0, &mut target_dir);
        } else {
            loop {
                if let Some((dirs_left, tot_dirs)) = stack.front_mut() {
                    target_dir.pop();
                    if *dirs_left > 0 {
                        *dirs_left -= 1;
                        cache.push_dir_name(*tot_dirs - *dirs_left, &mut target_dir);
                        break;
                    } else {
                        stack.pop_front();
                    }
                } else {
                    break 'outer;
                }
            }
        }

        if tasks.len() == tasks.capacity() {
            debug!("Flushing pending task queue.");
            for task in tasks.drain(..) {
                task.await
                    .with_context(|| "Failed to retrieve task result")
                    .with_code(exitcode::SOFTWARE)??;
            }
        }
    }

    for task in tasks {
        task.await
            .with_context(|| "Failed to retrieve task result")
            .with_code(exitcode::SOFTWARE)??;
    }

    cache.free();
    Ok(stats)
}

fn create_files_and_dirs(
    root_dir: PathBuf,
    num_files: usize,
    num_dirs: usize,
    cache: FileNameCache,
) -> CliResult<()> {
    debug!(
        "Creating {} files and {} directories in {:?}",
        num_files, num_dirs, root_dir,
    );

    let mut file = root_dir;

    for i in 0..num_dirs {
        cache.push_dir_name(i, &mut file);

        create_dir_all(&file)
            .with_context(|| format!("Failed to create directory {:?}", file))
            .with_code(exitcode::IOERR)?;

        file.pop();
    }

    let mut start_file = 0;
    if num_files > 0 {
        cache.push_file_name(0, &mut file);

        if let Err(e) = create_file(&file) {
            #[cfg(target_os = "linux")]
            let is_dir_missing = e == nix::errno::Errno::ENOENT;
            #[cfg(not(target_os = "linux"))]
            let is_dir_missing = e.kind() == std::io::ErrorKind::NotFound;

            if is_dir_missing {
                file.pop();
                create_dir_all(&file)
                    .with_context(|| format!("Failed to create directory {:?}", file))
                    .with_code(exitcode::IOERR)?;
            } else {
                return Err(e)
                    .with_context(|| format!("Failed to create file {:?}", file))
                    .with_code(exitcode::IOERR);
            }
        } else {
            start_file += 1;
            file.pop();
        }
    }
    for i in start_file..num_files {
        cache.push_file_name(i, &mut file);

        create_file(&file)
            .with_context(|| format!("Failed to create file {:?}", file))
            .with_code(exitcode::IOERR)?;

        file.pop();
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn create_file(file: &Path) -> nix::Result<()> {
    use nix::sys::stat::{mknod, Mode, SFlag};
    mknod(
        file,
        SFlag::S_IFREG,
        Mode::S_IRUSR | Mode::S_IWUSR | Mode::S_IRGRP | Mode::S_IWGRP | Mode::S_IROTH,
        0,
    )
}

#[cfg(not(target_os = "linux"))]
fn create_file(file: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(file)
}

trait GeneratorUtils {
    fn num_to_generate(self, random: &mut impl RngCore) -> usize;
}

impl GeneratorUtils for f64 {
    fn num_to_generate(self, random: &mut impl RngCore) -> usize {
        let sample = if self > 10_000. {
            LogNormal::from_mean_cv(self, 2.).unwrap().sample(random)
        } else {
            // LogNormal doesn't perform well with values under 10K for our purposes,
            // so default to normal
            Normal::new(self, self * 0.2).unwrap().sample(random)
        };

        sample.round() as usize
    }
}
