use anyhow::{bail, ensure, Context, Result};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};

/// Base-ten numbers keep the user's digits, including the YAML scale, exact.
#[derive(Clone, Debug)]
pub struct Decimal {
    coefficient: BigInt,
    exponent: i32,
}

impl Decimal {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(text.len() <= 1024, "decimal input exceeds 1024 characters");
        let (mantissa, exponent) = match text.find(['e', 'E']) {
            Some(i) => (
                &text[..i],
                text[i + 1..]
                    .parse::<i32>()
                    .context("invalid decimal exponent")?,
            ),
            None => (text, 0),
        };
        ensure!(
            (-1024..=1024).contains(&exponent),
            "decimal exponent must be between -1024 and 1024"
        );
        let unsigned = mantissa.trim_start_matches(['-', '+']);
        ensure!(!unsigned.is_empty(), "enter a decimal number");
        let mut digits = String::new();
        let mut fractional = None;
        for c in unsigned.chars() {
            match c {
                '0'..='9' => {
                    digits.push(c);
                    if let Some(count) = &mut fractional {
                        *count += 1;
                    }
                }
                '.' if fractional.is_none() => fractional = Some(0),
                _ => bail!("enter a decimal number (expressions are not supported)"),
            }
        }
        ensure!(!digits.is_empty(), "enter a decimal number");
        // Reject repeated signs; trim_start_matches above is only for finding digits.
        ensure!(
            !mantissa.get(1..).unwrap_or("").contains(['-', '+']),
            "invalid decimal sign"
        );
        let mut coefficient = digits.parse::<BigInt>()?;
        if mantissa.starts_with('-') {
            coefficient = -coefficient;
        }
        Ok(Self {
            coefficient,
            exponent: exponent - fractional.unwrap_or(0),
        })
    }

    fn rational(&self) -> BigRational {
        let power = BigInt::from(10).pow(self.exponent.unsigned_abs());
        if self.exponent >= 0 {
            BigRational::from_integer(&self.coefficient * power)
        } else {
            BigRational::new(self.coefficient.clone(), power)
        }
    }

    pub fn stored_integer(&self, scale: &Self) -> Result<i64> {
        ensure!(!scale.coefficient.is_zero(), "scale cannot be zero");
        let stored = self.rational() / scale.rational();
        ensure!(
            stored.is_integer(),
            "value / scale is fractional; enter an exactly representable stored integer"
        );
        stored
            .to_integer()
            .to_i64()
            .context("stored integer is out of range")
    }

    pub fn scaled_integer_text(&self, stored: i64) -> String {
        let coefficient = &self.coefficient * stored;
        let raw = coefficient.to_string();
        let (sign, digits) = if let Some(s) = raw.strip_prefix('-') {
            ("-", s)
        } else {
            ("", raw.as_str())
        };
        if coefficient.is_zero() {
            return "0".into();
        }
        if self.exponent >= 0 {
            return format!("{sign}{digits}{}", "0".repeat(self.exponent as usize));
        }
        let places = (-self.exponent) as usize;
        let mut result = if places >= digits.len() {
            format!("{sign}0.{}{digits}", "0".repeat(places - digits.len()))
        } else {
            let split = digits.len() - places;
            format!("{sign}{}.{}", &digits[..split], &digits[split..])
        };
        while result.ends_with('0') {
            result.pop();
        }
        if result.ends_with('.') {
            result.pop();
        }
        result
    }
}
