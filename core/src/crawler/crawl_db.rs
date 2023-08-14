// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap, HashMap, VecDeque},
    hash::Hash,
    io::Write,
    num::NonZeroUsize,
    ops::Range,
    path::Path,
    sync::{Arc, Mutex},
};

use itertools::Itertools;
use lru::LruCache;
use rand::Rng;
use rayon::prelude::IntoParallelRefIterator;
use rayon::prelude::*;
use rocksdb::BlockBasedOptions;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Domain, Job, JobResponse, Result, UrlResponse};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UrlStatus {
    Pending,
    Crawling,
    Failed { status_code: Option<u16> },
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainStatus {
    Pending,
    CrawlInProgress,
}

/// This is a simple key-value store that uses memory mapping to store the data on disk.
/// It is not thread safe, so it should be wrapped in a mutex.
///
/// The keys are stored in memory in a map from `K` to `Range<usize>`, where the range is the
/// position of the value in the file.
///
/// This DB should only be used for data that is acceptable to lose in case of a crash.
/// Currently the database doesn't even save the key -> range map to disk, so it will be lost.
struct MemmapDb<K, V> {
    inner: Option<memmap::MmapMut>,
    file: std::fs::File,
    ranges: BTreeMap<K, Range<usize>>,
    len: usize,

    write_batch: Vec<u8>,

    _phantom: std::marker::PhantomData<V>,
}

impl<K, V> MemmapDb<K, V>
where
    K: serde::Serialize + serde::de::DeserializeOwned + Hash + Ord + Eq + Clone,
    V: serde::Serialize + serde::de::DeserializeOwned + Clone,
{
    fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        std::fs::create_dir_all(path.as_ref())?;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_ref().join("db"))?;

        let inner = if file.metadata()?.len() > 0 {
            Some(unsafe { memmap::MmapOptions::new().map_mut(&file)? })
        } else {
            None
        };

        let ranges = BTreeMap::new();

        Ok(Self {
            inner,
            ranges,
            file,
            len: 0,
            write_batch: Vec::new(),
            _phantom: std::marker::PhantomData,
        })
    }

    fn get(&self, key: &K) -> Option<V> {
        let range = self.ranges.get(key)?;

        if range.start >= self.inner.as_ref()?.len() || range.end > self.inner.as_ref()?.len() {
            return None;
        }

        let value_bytes = &self.inner.as_ref()?[range.clone()];
        let value: V = bincode::deserialize(value_bytes).ok()?;

        Some(value)
    }

    fn put(&mut self, key: &K, value: &V) -> Result<()> {
        let value_bytes = bincode::serialize(value)?;

        let range = self.len..self.len + value_bytes.len();

        self.write_batch.write_all(&value_bytes)?;
        self.len += value_bytes.len();

        self.ranges.insert(key.clone(), range);

        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.file.write_all(&self.write_batch)?;
        self.write_batch.clear();

        if let Some(inner) = &self.inner {
            inner.flush()?;
        }

        self.file.flush()?;

        self.inner = if self.file.metadata()?.len() > 0 {
            Some(unsafe { memmap::MmapOptions::new().map_mut(&self.file)? })
        } else {
            None
        };

        Ok(())
    }

    fn contains(&self, key: &K) -> bool {
        self.ranges.contains_key(key)
    }
}

type Id = u128;
trait AsId {
    fn as_id(&self) -> Id;
}

impl AsId for Domain {
    fn as_id(&self) -> Id {
        self.id().0
    }
}

impl AsId for Url {
    fn as_id(&self) -> Id {
        let digest = md5::compute(self.as_str());
        u128::from_be_bytes(digest.0)
    }
}

struct IdTable<T> {
    db: Mutex<MemmapDb<Id, T>>,
    value_cache: Mutex<LruCache<Id, T>>,
    id_cache: Mutex<LruCache<T, Id>>,
}

impl<T> IdTable<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Hash + Eq + Clone + AsId,
{
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        // create dir if not exists
        std::fs::create_dir_all(path.as_ref())?;

        let db = Mutex::new(MemmapDb::open(path.as_ref().join("db"))?);

        Ok(Self {
            db,
            value_cache: Mutex::new(LruCache::new(NonZeroUsize::new(50_000).unwrap())),
            id_cache: Mutex::new(LruCache::new(NonZeroUsize::new(50_000).unwrap())),
        })
    }

    pub fn bulk_insert_ids(&self, items: &[(T, Id)]) -> Result<()> {
        let mut db = self.db.lock().unwrap();

        let mut id_cache = self.id_cache.lock().unwrap();

        for (item, id) in items {
            if !db.contains(id) {
                db.put(id, item)?;
            }

            id_cache.put(item.clone(), *id);
        }

        db.commit()?;

        Ok(())
    }

    pub fn id(&self, item: &T) -> Result<Id> {
        if let Some(id) = self.id_cache.lock().unwrap().get(item) {
            return Ok(*id);
        }

        let id = item.as_id();

        let mut db = self.db.lock().unwrap();

        // check if item exists
        if !db.contains(&id) {
            db.put(&id, item)?;
        }

        Ok(id)
    }

    pub fn value(&mut self, id: Id) -> Result<Option<T>> {
        let mut cache = self.value_cache.lock().unwrap();

        // check cache
        if let Some(value) = cache.get(&id) {
            return Ok(Some(value.clone()));
        }

        // check db
        let value = self.db.lock().unwrap().get(&id);

        if let Some(value) = &value {
            cache.put(id, value.clone());
        }

        Ok(value)
    }
}

struct SampledItem<'a, T> {
    item: &'a T,
    priority: f64,
}

impl<'a, T> PartialEq for SampledItem<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl<'a, T> Eq for SampledItem<'a, T> {}

impl<'a, T> PartialOrd for SampledItem<'a, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl<'a, T> Ord for SampledItem<'a, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .partial_cmp(&other.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

fn weighted_sample<'a, T: 'a>(
    items: impl Iterator<Item = (&'a T, f64)>,
    num_items: usize,
) -> Vec<&'a T> {
    let mut sampled_items: BinaryHeap<SampledItem<T>> = BinaryHeap::with_capacity(num_items);

    let mut rng = rand::thread_rng();

    for (item, weight) in items {
        // see https://www.kaggle.com/code/kotamori/random-sample-with-weights-on-sql/notebook for details on math
        let priority = -(rng.gen::<f64>().abs() + f64::EPSILON).ln() / (weight + 1.0);

        if sampled_items.len() < num_items {
            sampled_items.push(SampledItem { item, priority });
        } else if let Some(mut max) = sampled_items.peek_mut() {
            if priority < max.priority {
                max.item = item;
                max.priority = priority;
            }
        }
    }

    sampled_items.into_iter().map(|s| s.item).collect()
}

#[derive(Clone, Serialize, Deserialize)]
struct UrlState {
    weight: f64,
    status: UrlStatus,
}
struct DomainState {
    weight: f64,
    status: DomainStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DomainId(u128);

impl From<u128> for DomainId {
    fn from(id: u128) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
struct UrlId(u128);

impl From<u128> for UrlId {
    fn from(id: u128) -> Self {
        Self(id)
    }
}

pub struct RedirectDb {
    inner: rocksdb::DB,
}

impl RedirectDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);

        let mut block_options = BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        options.set_block_based_table_factory(&block_options);

        let inner = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { inner })
    }

    pub fn put(&self, from: &Url, to: &Url) -> Result<()> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = bincode::serialize(to)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.set_sync(false);
        write_options.disable_wal(true);
        self.inner
            .put_opt(url_bytes, redirect_bytes, &write_options)?;

        Ok(())
    }

    pub fn get(&self, from: &Url) -> Result<Option<Url>> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = self.inner.get(url_bytes)?;

        if let Some(redirect_bytes) = redirect_bytes {
            let redirect: Url = bincode::deserialize(&redirect_bytes)?;
            return Ok(Some(redirect));
        }

        Ok(None)
    }
}

struct UrlToInsert {
    url: Url,
    different_domain: bool,
}

struct UrlStateDb {
    cache: LruCache<DomainId, Arc<Mutex<BTreeMap<UrlId, UrlState>>>>,
    cache_size: usize,
    db: rocksdb::DB,
}

impl UrlStateDb {
    pub fn open<P: AsRef<Path>>(path: P, cache_size: usize) -> Result<Self> {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);

        let mut block_options = BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        options.set_block_based_table_factory(&block_options);
        options.optimize_for_point_lookup(512); // 512 MB
        options.set_optimize_filters_for_hits(true);

        let db = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self {
            cache: LruCache::new(NonZeroUsize::new(cache_size + 1).unwrap()),
            cache_size,
            db,
        })
    }

    pub fn get(&mut self, domain_id: DomainId) -> Result<Arc<Mutex<BTreeMap<UrlId, UrlState>>>> {
        // check cache
        if let Some(value) = self.cache.get(&domain_id) {
            return Ok(value.clone());
        }

        // check db
        let domain_id_bytes = bincode::serialize(&domain_id)?;
        let value_bytes = self.db.get(domain_id_bytes)?;

        if let Some(value_bytes) = &value_bytes {
            let value: Arc<Mutex<BTreeMap<UrlId, UrlState>>> = bincode::deserialize(value_bytes)?;
            self.cache.put(domain_id, value.clone());

            self.maybe_prune_cache()?;

            return Ok(value);
        }

        Ok(Arc::new(Mutex::new(BTreeMap::new())))
    }

    fn maybe_prune_cache(&mut self) -> Result<()> {
        // if cache is full, write half of it to disk
        if self.cache.len() >= self.cache_size {
            let mut batch = rocksdb::WriteBatch::default();

            for _ in 0..self.cache_size / 2 {
                if let Some((key, value)) = self.cache.pop_lru() {
                    let domain_id_bytes = bincode::serialize(&key)?;
                    let value_bytes = bincode::serialize(&value)?;

                    batch.put(domain_id_bytes, value_bytes);
                } else {
                    break;
                }
            }

            let mut write_options = rocksdb::WriteOptions::default();
            write_options.set_sync(false);
            write_options.disable_wal(true);

            self.db.write_opt(batch, &write_options)?;
        }

        Ok(())
    }

    pub fn put(
        &mut self,
        domain_id: DomainId,
        url_states: Arc<Mutex<BTreeMap<UrlId, UrlState>>>,
    ) -> Result<()> {
        self.cache.put(domain_id, url_states.clone());
        self.maybe_prune_cache()?;

        Ok(())
    }
}

pub struct CrawlDb {
    url_ids: IdTable<Url>,
    domain_ids: IdTable<Domain>,

    redirects: RedirectDb,

    domain_state: BTreeMap<DomainId, DomainState>,

    urls: UrlStateDb,
}

impl CrawlDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let url_ids = IdTable::open(path.as_ref().join("urls"))?;
        let domain_ids = IdTable::open(path.as_ref().join("domains"))?;
        let redirects = RedirectDb::open(path.as_ref().join("redirects"))?;

        Ok(Self {
            url_ids,
            domain_ids,
            redirects,
            domain_state: BTreeMap::new(),
            urls: UrlStateDb::open(path.as_ref().join("urls").join("states"), 10_000)?,
        })
    }

    pub fn insert_seed_urls(&mut self, urls: &[Url]) -> Result<()> {
        for url in urls {
            let domain_id = self.domain_ids.id(&Domain::from(url))?.into();
            let url_id = self.url_ids.id(url)?.into();

            self.domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            let urls = self.urls.get(domain_id)?;

            urls.lock().unwrap().insert(
                url_id,
                UrlState {
                    weight: 0.0,
                    status: UrlStatus::Pending,
                },
            );

            self.urls.put(domain_id, urls)?;
        }

        Ok(())
    }

    pub fn insert_urls(&mut self, responses: &[JobResponse]) -> Result<()> {
        let mut domains: HashMap<Domain, Vec<UrlToInsert>> = HashMap::new();

        responses.iter().for_each(|res| {
            for url in &res.discovered_urls {
                let domain = Domain::from(url);
                let different_domain = res.domain != domain;

                domains.entry(domain).or_default().push(UrlToInsert {
                    url: url.clone(),
                    different_domain,
                });
            }
        });

        let domain_ids: Vec<_> = domains
            .par_iter()
            .map(|(domain, _)| (domain.clone(), domain.as_id()))
            .collect();
        self.domain_ids.bulk_insert_ids(&domain_ids)?;

        let url_ids: Vec<_> = domains
            .par_iter()
            .flat_map(|(_, urls)| {
                urls.iter()
                    .map(|url| (url.url.clone(), url.url.as_id()))
                    .collect_vec()
            })
            .collect();
        self.url_ids.bulk_insert_ids(&url_ids)?;

        for (domain_id, urls) in domain_ids
            .into_iter()
            .map(|(_, id)| id)
            .zip_eq(domains.values())
        {
            let domain_id = domain_id.into();

            let domain_state = self
                .domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            let url_states = self.urls.get(domain_id)?;

            {
                let mut url_states = url_states.lock().unwrap();
                for url in urls {
                    let id = self.url_ids.id(&url.url);

                    if id.is_err() {
                        continue;
                    }

                    let id = id.unwrap();

                    let url_id: UrlId = id.into();

                    let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                        weight: 0.0,
                        status: UrlStatus::Pending,
                    });

                    if url.different_domain {
                        url_state.weight += 1.0;
                    }

                    if url_state.weight > domain_state.weight {
                        domain_state.weight = url_state.weight;
                    }
                }
            }

            self.urls.put(domain_id, url_states)?;
        }

        Ok(())
    }

    pub fn update_url_status(&mut self, job_responses: &[JobResponse]) -> Result<()> {
        let mut url_responses: HashMap<Domain, Vec<UrlResponse>> = HashMap::new();

        for res in job_responses {
            for url_response in &res.url_responses {
                match url_response {
                    UrlResponse::Success { url } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                    UrlResponse::Failed {
                        url,
                        status_code: _,
                    } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                    UrlResponse::Redirected { url, new_url: _ } => {
                        let domain = Domain::from(url);
                        url_responses
                            .entry(domain)
                            .or_default()
                            .push(url_response.clone());
                    }
                }
            }
        }

        // bulk register urls
        let url_ids: Vec<_> = url_responses
            .par_iter()
            .flat_map(|(_, responses)| {
                responses
                    .iter()
                    .flat_map(|res| match res {
                        UrlResponse::Success { url } => vec![url],
                        UrlResponse::Failed {
                            url,
                            status_code: _,
                        } => vec![url],
                        UrlResponse::Redirected { url, new_url } => vec![url, new_url],
                    })
                    .map(|url| (url.clone(), url.as_id()))
                    .collect_vec()
            })
            .collect();

        self.url_ids.bulk_insert_ids(&url_ids)?;

        // bulk register domains
        let domain_ids: Vec<_> = url_responses
            .par_iter()
            .map(|(domain, _)| (domain.clone(), domain.as_id()))
            .collect();
        self.domain_ids.bulk_insert_ids(&domain_ids)?;

        for (domain_id, responses) in url_responses.into_iter().filter_map(|(domain, responses)| {
            let domain_id: DomainId = self.domain_ids.id(&domain).ok()?.into();
            Some((domain_id, responses))
        }) {
            self.domain_state
                .entry(domain_id)
                .or_insert_with(|| DomainState {
                    weight: 0.0,
                    status: DomainStatus::Pending,
                });

            let url_states = self.urls.get(domain_id)?;
            {
                let mut url_states = url_states.lock().unwrap();
                for response in responses {
                    match response {
                        UrlResponse::Success { url } => {
                            let id = self.url_ids.id(&url);

                            if id.is_err() {
                                continue;
                            }

                            let url_id: UrlId = id.unwrap().into();

                            let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                                weight: 0.0,
                                status: UrlStatus::Pending,
                            });

                            url_state.status = UrlStatus::Done;
                        }
                        UrlResponse::Failed { url, status_code } => {
                            let id = self.url_ids.id(&url);

                            if id.is_err() {
                                continue;
                            }

                            let url_id: UrlId = id.unwrap().into();

                            let url_state = url_states.entry(url_id).or_insert_with(|| UrlState {
                                weight: 0.0,
                                status: UrlStatus::Pending,
                            });

                            url_state.status = UrlStatus::Failed { status_code };
                        }
                        UrlResponse::Redirected { url, new_url } => {
                            self.redirects.put(&url, &new_url).ok();
                        }
                    }
                }
            }

            self.urls.put(domain_id, url_states)?;
        }

        Ok(())
    }

    pub fn set_domain_status(&mut self, domain: &Domain, status: DomainStatus) -> Result<()> {
        let domain_id: DomainId = self.domain_ids.id(domain)?.into();

        let domain_state = self
            .domain_state
            .entry(domain_id)
            .or_insert_with(|| DomainState {
                weight: 0.0,
                status: status.clone(),
            });

        domain_state.status = status;

        Ok(())
    }

    pub fn sample_domains(&mut self, num_jobs: usize) -> Result<Vec<DomainId>> {
        let available_domains: Vec<_> = self
            .domain_state
            .iter()
            .filter_map(|(id, state)| {
                if state.status == DomainStatus::Pending {
                    Some((*id, state.weight))
                } else {
                    None
                }
            })
            .collect();

        let sampled = weighted_sample(
            available_domains.iter().map(|(id, weight)| (id, *weight)),
            num_jobs,
        )
        .into_iter()
        .copied()
        .collect();

        for id in &sampled {
            let state = self.domain_state.get_mut(id).unwrap();
            state.status = DomainStatus::CrawlInProgress;
        }

        Ok(sampled)
    }

    pub fn prepare_jobs(&mut self, domains: &[DomainId], urls_per_job: usize) -> Result<Vec<Job>> {
        let mut jobs = Vec::with_capacity(domains.len());
        for domain_id in domains {
            let urls = self.urls.get(*domain_id)?;

            {
                let mut urls = urls.lock().unwrap();

                let sampled: Vec<_> = weighted_sample(
                    urls.iter().filter_map(|(id, state)| {
                        if state.status == UrlStatus::Pending {
                            Some((id, state.weight))
                        } else {
                            None
                        }
                    }),
                    urls_per_job,
                )
                .into_iter()
                .copied()
                .collect();

                for id in &sampled {
                    let state = urls.get_mut(id).unwrap();
                    state.status = UrlStatus::Crawling;
                }

                let domain_state = self.domain_state.get_mut(domain_id).unwrap();

                domain_state.weight = urls
                    .iter()
                    .filter_map(|(_, state)| {
                        if state.status == UrlStatus::Pending {
                            Some(state.weight)
                        } else {
                            None
                        }
                    })
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                    .unwrap_or(0.0);

                let mut job = Job {
                    domain: self.domain_ids.value(domain_id.0)?.unwrap(),
                    fetch_sitemap: false, // todo: fetch for new sites
                    urls: VecDeque::with_capacity(urls_per_job),
                };

                for url_id in sampled {
                    let url = self.url_ids.value(url_id.0)?.unwrap();
                    job.urls.push_back(url);
                }

                jobs.push(job);
            }

            self.urls.put(*domain_id, urls)?;
        }

        Ok(jobs)
    }
}

#[cfg(test)]
mod tests {
    use crate::gen_temp_path;

    use super::*;

    #[test]
    fn sampling() {
        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 10);
        assert_eq!(sampled.len(), items.len());

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 0);
        assert_eq!(sampled.len(), 0);

        let items: Vec<(usize, f64)> = vec![(0, 1000000000.0), (1, 2.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);
        assert_eq!(*sampled[0], 0);
    }

    #[test]
    fn memmap_db() {
        let mut db: MemmapDb<u128, String> = MemmapDb::open(gen_temp_path()).unwrap();

        assert!(!db.contains(&123));
        assert!(db.get(&123).is_none());

        db.put(&123, &"hello".to_string()).unwrap();
        db.commit().unwrap();

        assert!(db.contains(&123));
        assert_eq!(db.get(&123).unwrap(), "hello");

        db.put(&321, &"world".to_string()).unwrap();

        assert!(db.contains(&321));
        assert!(db.get(&321).is_none());

        db.commit().unwrap();

        assert!(db.contains(&321));
        assert_eq!(db.get(&321).unwrap(), "world");
    }

    #[test]
    fn simple_politeness() {
        let mut db = CrawlDb::open(gen_temp_path()).unwrap();

        db.insert_seed_urls(&[Url::parse("https://example.com").unwrap()])
            .unwrap();

        let domain = Domain::from(&Url::parse("https://example.com").unwrap());
        let domain_id = db.domain_ids.id(&domain).unwrap().into();
        let sample = db.sample_domains(128).unwrap();

        assert_eq!(sample.len(), 1);
        assert_eq!(&sample[0], &domain_id);
        assert_eq!(
            db.domain_state.get(&domain_id).unwrap().status,
            DomainStatus::CrawlInProgress
        );

        let new_sample = db.sample_domains(128).unwrap();
        assert_eq!(new_sample.len(), 0);
    }
}
