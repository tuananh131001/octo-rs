//! `System.Random` constructed with a seed: the framework type behind the radio's weighted draw
//! (`LastFmRadioRecommendationService.Randomizer`) and the mixes' per-period shuffle
//! (`GeneratedPlaylistService.Select`). Not a C# file of Octo's.
//!
//! A seeded `new Random(seed)` in .NET 6+ still runs the .NET Framework algorithm (Knuth's
//! subtractive generator, `Net5CompatSeedImpl`), so the same seed gives the same sequence on
//! every .NET version. Porting it exactly means a mix drawn by the Rust build for a listener and
//! a period holds the same songs the C# build drew, and the C# tests' seeds draw what they drew.

/// `new Random(seed)`.
#[derive(Debug, Clone)]
pub struct DotnetRandom {
    seed_array: [i32; 56],
    inext: usize,
    inextp: usize,
}

const MSEED: i32 = 161_803_398;

impl DotnetRandom {
    pub fn new(seed: i32) -> Self {
        let mut seed_array = [0i32; 56];
        let subtraction = if seed == i32::MIN { i32::MAX } else { seed.abs() };
        let mut mj = MSEED.wrapping_sub(subtraction);
        seed_array[55] = mj;
        let mut mk = 1i32;
        let mut ii = 0usize;
        // The range [1..55] is special (Knuth) and so the 0th position is wasted.
        for _ in 1..55 {
            ii += 21;
            if ii >= 55 {
                ii -= 55;
            }
            seed_array[ii] = mk;
            mk = mj.wrapping_sub(mk);
            if mk < 0 {
                mk = mk.wrapping_add(i32::MAX);
            }
            mj = seed_array[ii];
        }
        for _ in 1..5 {
            for i in 1..56 {
                let mut n = i + 30;
                if n >= 55 {
                    n -= 55;
                }
                seed_array[i] = seed_array[i].wrapping_sub(seed_array[1 + n]);
                if seed_array[i] < 0 {
                    seed_array[i] = seed_array[i].wrapping_add(i32::MAX);
                }
            }
        }
        DotnetRandom {
            seed_array,
            inext: 0,
            inextp: 21,
        }
    }

    fn internal_sample(&mut self) -> i32 {
        let mut next = self.inext + 1;
        if next >= 56 {
            next = 1;
        }
        let mut nextp = self.inextp + 1;
        if nextp >= 56 {
            nextp = 1;
        }
        let mut value = self.seed_array[next].wrapping_sub(self.seed_array[nextp]);
        if value == i32::MAX {
            value -= 1;
        }
        if value < 0 {
            value = value.wrapping_add(i32::MAX);
        }
        self.seed_array[next] = value;
        self.inext = next;
        self.inextp = nextp;
        value
    }

    /// `NextDouble()`: in [0, 1).
    pub fn next_double(&mut self) -> f64 {
        f64::from(self.internal_sample()) * (1.0 / f64::from(i32::MAX))
    }

    /// `Next(maxValue)`: in [0, maxValue). (`maxValue` must not be negative, as C# required.)
    pub fn next(&mut self, max_value: i32) -> i32 {
        (self.next_double() * f64::from(max_value)) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequences .NET 9 gave for these seeds (printed with "R").
    #[test]
    fn draws_what_dotnet_draws_for_a_seed() {
        let cases: [(i32, [f64; 6], [i32; 12]); 7] = [
            (
                0,
                [
                    0.7262432699679598,
                    0.8173253595909687,
                    0.7680226893946634,
                    0.5581611914365372,
                    0.2060331540210327,
                    0.5588847946184151,
                ],
                [5, 11, 16, 15, 7, 23, 44, 24, 61, 19, 22, 39],
            ),
            (
                1,
                [
                    0.24866858415709278,
                    0.11074397718102856,
                    0.46701067987224587,
                    0.7716041220219825,
                    0.657518893786482,
                    0.43278260130099144,
                ],
                [1, 1, 9, 21, 23, 18, 17, 52, 6, 44, 2, 20],
            ),
            (
                42,
                [
                    0.6681064659115423,
                    0.14090729837348093,
                    0.12551828945312568,
                    0.5227642760252413,
                    0.16843422416990353,
                    0.26259267528662117,
                ],
                [4, 1, 2, 14, 5, 11, 35, 28, 10, 53, 18, 21],
            ),
            (
                1234,
                [
                    0.39908097935797693,
                    0.8958994657247791,
                    0.3192029387313886,
                    0.9467375338760845,
                    0.33943602458547617,
                    0.9487782409176129,
                ],
                [2, 12, 6, 26, 11, 39, 39, 29, 40, 21, 32, 72],
            ),
            (
                -7,
                [
                    0.38322046929189024,
                    0.8712556827213874,
                    0.6609386227377405,
                    0.052261705534654534,
                    0.36643333237917786,
                    0.6761694413964494,
                ],
                [2, 12, 13, 1, 12, 28, 2, 53, 53, 59, 34, 78],
            ),
            (
                i32::MIN,
                [
                    0.7262432699679598,
                    0.8173253595909687,
                    0.7680226921886312,
                    0.5581611914365372,
                    0.2060331540210327,
                    0.5588847936870925,
                ],
                [5, 11, 16, 15, 7, 23, 44, 24, 61, 19, 22, 39],
            ),
            (
                i32::MAX,
                [
                    0.7262432699679598,
                    0.8173253595909687,
                    0.7680226921886312,
                    0.5581611914365372,
                    0.2060331540210327,
                    0.5588847936870925,
                ],
                [5, 11, 16, 15, 7, 23, 44, 24, 61, 19, 22, 39],
            ),
        ];
        for (seed, doubles, nexts) in cases {
            let mut random = DotnetRandom::new(seed);
            let drawn: Vec<f64> = (0..6).map(|_| random.next_double()).collect();
            assert_eq!(drawn, doubles, "NextDouble, seed {seed}");
            let mut random = DotnetRandom::new(seed);
            let drawn: Vec<i32> = (1..=12).map(|i| random.next(i * 7)).collect();
            assert_eq!(drawn, nexts, "Next, seed {seed}");
        }
    }
}
