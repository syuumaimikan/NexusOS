use super::{Error, ModelProvider};
use alloc::{vec, vec::Vec};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Config {
    dim: usize,
    hidden: usize,
    layers: usize,
    heads: usize,
    kv_heads: usize,
    vocab: usize,
    seq: usize,
}
impl Config {
    fn kv(self) -> usize {
        self.dim * self.kv_heads / self.heads
    }
}
struct Weights {
    emb: Vec<f32>,
    att: Vec<f32>,
    q: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
    o: Vec<f32>,
    ffn: Vec<f32>,
    w1: Vec<f32>,
    w2: Vec<f32>,
    w3: Vec<f32>,
    norm: Vec<f32>,
    cls: Option<Vec<f32>>,
}
pub struct Llama {
    c: Config,
    w: Weights,
}
pub struct State {
    config: Config,
    pos: usize,
    x: Vec<f32>,
    xb: Vec<f32>,
    xb2: Vec<f32>,
    h: Vec<f32>,
    h2: Vec<f32>,
    q: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
    att: Vec<f32>,
    logits: Vec<f32>,
}

impl Llama {
    /// Checked little-endian legacy llama2.c f32 checkpoint, capped at 16 MiB.
    /// Reject unsupported geometry and nonfinite weights before inference.
    pub fn load(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 28 || bytes.len() > 16 * 1024 * 1024 {
            return Err(Error::InvalidModel);
        }
        let mut fields = [0i32; 7];
        for (i, f) in fields.iter_mut().enumerate() {
            *f = i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        if fields.iter().enumerate().any(|(i, &v)| v <= 0 && i != 5)
            || fields[5] == 0
            || fields[5] == i32::MIN
        {
            return Err(Error::InvalidModel);
        }
        let c = Config {
            dim: fields[0] as usize,
            hidden: fields[1] as usize,
            layers: fields[2] as usize,
            heads: fields[3] as usize,
            kv_heads: fields[4] as usize,
            vocab: fields[5].unsigned_abs() as usize,
            seq: fields[6] as usize,
        };
        if c.dim > 256
            || c.hidden > 1024
            || c.layers > 8
            || c.heads > c.dim
            || c.kv_heads > c.heads
            || c.vocab < 259
            || c.vocab > 4096
            || c.seq > 512
            || !c.dim.is_multiple_of(c.heads)
            || !c.heads.is_multiple_of(c.kv_heads)
            || !(c.dim / c.heads).is_multiple_of(2)
        {
            return Err(Error::InvalidModel);
        }
        let (d, h, l, kv) = (c.dim, c.hidden, c.layers, c.kv());
        let count = c.vocab * d
            + 2 * l * d
            + 2 * l * d * d
            + 2 * l * d * kv
            + 3 * l * d * h
            + d
            + c.seq * (d / c.heads)
            + if fields[5] < 0 { c.vocab * d } else { 0 };
        if bytes.len() != 28 + count * 4 {
            return Err(Error::InvalidModel);
        }
        // Validate all weights before allocating. Bounds above make size math safe.
        if bytes[28..]
            .as_chunks::<4>()
            .0
            .iter()
            .any(|v| !f32::from_le_bytes(*v).is_finite())
        {
            return Err(Error::InvalidModel);
        }
        let mut at = 28;
        let mut take = |n: usize| {
            let result = bytes[at..at + n * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| f32::from_le_bytes(*v))
                .collect();
            at += n * 4;
            result
        };
        let w = Weights {
            emb: take(c.vocab * d),
            att: take(l * d),
            q: take(l * d * d),
            k: take(l * d * kv),
            v: take(l * d * kv),
            o: take(l * d * d),
            ffn: take(l * d),
            w1: take(l * d * h),
            w2: take(l * d * h),
            w3: take(l * d * h),
            norm: take(d),
            cls: None,
        };
        at += c.seq * (d / c.heads) * 4; // obsolete serialized RoPE tables
        let cls = if fields[5] < 0 {
            Some(
                bytes[at..]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|v| f32::from_le_bytes(*v))
                    .collect(),
            )
        } else {
            None
        };
        Ok(Self {
            c,
            w: Weights { cls, ..w },
        })
    }
}
fn mat(out: &mut [f32], x: &[f32], w: &[f32]) -> Result<(), Error> {
    for (i, y) in out.iter_mut().enumerate() {
        let mut sum = 0.;
        for (j, &v) in x.iter().enumerate() {
            sum += w[i * x.len() + j] * v;
        }
        if !sum.is_finite() {
            return Err(Error::Numerical);
        }
        *y = sum;
    }
    Ok(())
}
fn norm(out: &mut [f32], x: &[f32], w: &[f32]) -> Result<(), Error> {
    let mut sum = 0.;
    for &v in x {
        sum += v * v;
    }
    if !sum.is_finite() {
        return Err(Error::Numerical);
    }
    let scale = 1. / libm::sqrtf(sum / x.len() as f32 + 1e-5);
    for i in 0..x.len() {
        out[i] = w[i] * (scale * x[i]);
        if !out[i].is_finite() {
            return Err(Error::Numerical);
        }
    }
    Ok(())
}
fn softmax(x: &mut [f32]) -> Result<(), Error> {
    if x.iter().any(|v| !v.is_finite()) {
        return Err(Error::Numerical);
    }
    let max = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.;
    for v in x.iter_mut() {
        *v = libm::expf(*v - max);
        sum += *v;
    }
    if !sum.is_finite() || sum <= 0. {
        return Err(Error::Numerical);
    }
    for v in x {
        *v /= sum;
    }
    Ok(())
}
impl ModelProvider for Llama {
    type State = State;
    fn context_len(&self) -> usize {
        self.c.seq
    }
    fn vocab_size(&self) -> usize {
        self.c.vocab
    }
    fn new_state(&self) -> State {
        let c = self.c;
        State {
            config: c,
            pos: 0,
            x: vec![0.; c.dim],
            xb: vec![0.; c.dim],
            xb2: vec![0.; c.dim],
            h: vec![0.; c.hidden],
            h2: vec![0.; c.hidden],
            q: vec![0.; c.dim],
            k: vec![0.; c.layers * c.seq * c.kv()],
            v: vec![0.; c.layers * c.seq * c.kv()],
            att: vec![0.; c.seq],
            logits: vec![0.; c.vocab],
        }
    }
    fn next(&self, s: &mut State, token: u32) -> Result<u32, Error> {
        let c = self.c;
        let w = &self.w;
        if s.config != c {
            return Err(Error::InvalidRequest);
        }
        let (d, kv, h, hs, pos) = (c.dim, c.kv(), c.hidden, c.dim / c.heads, s.pos);
        if token as usize >= c.vocab {
            return Err(Error::InvalidRequest);
        }
        if pos >= c.seq {
            return Err(Error::Limit);
        }
        s.x.copy_from_slice(&w.emb[token as usize * d..(token as usize + 1) * d]);
        for l in 0..c.layers {
            norm(&mut s.xb, &s.x, &w.att[l * d..(l + 1) * d])?;
            let lo = l * c.seq * kv;
            let at = lo + pos * kv;
            mat(&mut s.q, &s.xb, &w.q[l * d * d..])?;
            mat(&mut s.k[at..at + kv], &s.xb, &w.k[l * d * kv..])?;
            mat(&mut s.v[at..at + kv], &s.xb, &w.v[l * d * kv..])?;
            for i in (0..d).step_by(2) {
                let angle = pos as f32 / libm::powf(10000., (i % hs) as f32 / hs as f32);
                let (co, si) = (libm::cosf(angle), libm::sinf(angle));
                let (a, b) = (s.q[i], s.q[i + 1]);
                s.q[i] = a * co - b * si;
                s.q[i + 1] = a * si + b * co;
                if i < kv {
                    let (a, b) = (s.k[at + i], s.k[at + i + 1]);
                    s.k[at + i] = a * co - b * si;
                    s.k[at + i + 1] = a * si + b * co;
                }
            }
            for head in 0..c.heads {
                let offset = head / (c.heads / c.kv_heads) * hs;
                for t in 0..=pos {
                    let mut score = 0.;
                    for i in 0..hs {
                        score += s.q[head * hs + i] * s.k[lo + t * kv + offset + i];
                    }
                    s.att[t] = score / libm::sqrtf(hs as f32);
                }
                softmax(&mut s.att[..=pos])?;
                s.xb[head * hs..(head + 1) * hs].fill(0.);
                for t in 0..=pos {
                    for i in 0..hs {
                        s.xb[head * hs + i] += s.att[t] * s.v[lo + t * kv + offset + i];
                    }
                }
            }
            mat(&mut s.xb2, &s.xb, &w.o[l * d * d..])?;
            for i in 0..d {
                s.x[i] += s.xb2[i];
            }
            norm(&mut s.xb, &s.x, &w.ffn[l * d..(l + 1) * d])?;
            mat(&mut s.h, &s.xb, &w.w1[l * d * h..])?;
            mat(&mut s.h2, &s.xb, &w.w3[l * d * h..])?;
            for i in 0..h {
                s.h[i] *= 1. / (1. + libm::expf(-s.h[i]));
                s.h[i] *= s.h2[i];
            }
            mat(&mut s.xb, &s.h, &w.w2[l * d * h..])?;
            for i in 0..d {
                s.x[i] += s.xb[i];
            }
        }
        norm(&mut s.xb, &s.x, &w.norm)?;
        mat(&mut s.logits, &s.xb, w.cls.as_ref().unwrap_or(&w.emb))?;
        if s.logits.iter().any(|v| !v.is_finite()) {
            return Err(Error::Numerical);
        }
        let mut best = 0;
        for i in 1..c.vocab {
            if s.logits[i] > s.logits[best] {
                best = i;
            }
        }
        s.pos += 1;
        Ok(best as u32)
    }
}
