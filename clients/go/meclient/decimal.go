package meclient

import (
	"fmt"
	"math"
	"math/big"
	"strings"
)

// Scale is the fixed-point scaling factor (10^8).
const Scale int64 = 100_000_000

// Decimal represents a fixed-point decimal number with 8 decimal places of
// precision. Internally it stores the value multiplied by 10^8.
type Decimal struct {
	raw int64
}

// NewDecimal creates a Decimal from a float64 value.
//
// WARNING: float64 has only ~15-16 significant decimal digits, so many exact
// decimal values (e.g. 0.07, 12345.67) are NOT representable and this
// constructor introduces rounding error. For monetary values (prices,
// quantities) use NewDecimalFromString, which parses without any float
// intermediate. NewDecimal is retained only for convenience in tests and
// precision-insensitive code.
func NewDecimal(f float64) Decimal {
	return Decimal{raw: int64(math.Round(f * float64(Scale)))}
}

// isDigits reports whether s consists solely of ASCII digits. An empty string
// is considered valid (an absent integer or fractional part).
func isDigits(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

// NewDecimalFromString parses a decimal string (e.g. "12345.67", "-0.00000001")
// into a fixed-point Decimal WITHOUT any float64 intermediate, so no precision
// is lost. At most 8 fractional digits are allowed. This is the canonical
// constructor for monetary values and mirrors the server's exact parser.
func NewDecimalFromString(s string) (Decimal, error) {
	s = strings.TrimSpace(s)
	neg := false
	switch {
	case strings.HasPrefix(s, "-"):
		neg = true
		s = s[1:]
	case strings.HasPrefix(s, "+"):
		s = s[1:]
	}

	intStr := s
	fracStr := ""
	if i := strings.IndexByte(s, '.'); i >= 0 {
		intStr = s[:i]
		fracStr = s[i+1:]
	}
	if intStr == "" && fracStr == "" {
		return Decimal{}, fmt.Errorf("invalid decimal %q: empty number", s)
	}
	if len(fracStr) > 8 {
		return Decimal{}, fmt.Errorf("invalid decimal %q: too many decimal places: %d (max 8)", s, len(fracStr))
	}
	// Reject anything that is not a run of ASCII digits in each part. This
	// catches embedded signs ("--1"), extra dots ("1.2.3" leaves "2.3" here),
	// and stray characters that big.Int.SetString would otherwise tolerate
	// (it accepts a leading sign).
	if !isDigits(intStr) || !isDigits(fracStr) {
		return Decimal{}, fmt.Errorf("invalid decimal %q: non-digit characters", s)
	}

	raw := new(big.Int)
	if intStr != "" {
		ip, ok := new(big.Int).SetString(intStr, 10)
		if !ok {
			return Decimal{}, fmt.Errorf("invalid decimal %q: bad integer part", s)
		}
		raw.Mul(ip, big.NewInt(Scale))
	}
	if fracStr != "" {
		fp, ok := new(big.Int).SetString(fracStr, 10)
		if !ok {
			return Decimal{}, fmt.Errorf("invalid decimal %q: bad fractional part", s)
		}
		// Scale the fractional digits up to 8 places: e.g. "5" (1 digit) means
		// 5 * 10^(8-1) = 50_000_000.
		scaleFactor := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(8-len(fracStr))), nil)
		fp.Mul(fp, scaleFactor)
		raw.Add(raw, fp)
	}
	if neg {
		raw.Neg(raw)
	}
	if !raw.IsInt64() {
		return Decimal{}, fmt.Errorf("invalid decimal %q: out of range for fixed-point int64", s)
	}
	return Decimal{raw: raw.Int64()}, nil
}

// NewDecimalRaw creates a Decimal from a raw fixed-point integer.
func NewDecimalRaw(raw int64) Decimal {
	return Decimal{raw: raw}
}

// NewDecimalFromUint64 creates a Decimal from a raw uint64 value.
// This is used for quantity fields which are unsigned in the wire protocol.
func NewDecimalFromUint64(raw uint64) Decimal {
	return Decimal{raw: int64(raw)}
}

// ToFloat64 converts the Decimal to a float64.
func (d Decimal) ToFloat64() float64 {
	return float64(d.raw) / float64(Scale)
}

// Raw returns the underlying raw fixed-point integer value.
func (d Decimal) Raw() int64 {
	return d.raw
}

// RawUint64 returns the underlying raw value as a uint64.
// This is used for quantity fields which are unsigned in the wire protocol.
func (d Decimal) RawUint64() uint64 {
	return uint64(d.raw)
}

// String returns a human-readable string representation of the decimal.
func (d Decimal) String() string {
	sign := ""
	v := d.raw
	if v < 0 {
		sign = "-"
		v = -v
	}
	whole := v / Scale
	frac := v % Scale
	return fmt.Sprintf("%s%d.%08d", sign, whole, frac)
}

// IsZero returns true if the decimal value is zero.
func (d Decimal) IsZero() bool {
	return d.raw == 0
}

// Add returns the sum of two decimals.
func (d Decimal) Add(other Decimal) Decimal {
	return Decimal{raw: d.raw + other.raw}
}

// Sub returns the difference of two decimals.
func (d Decimal) Sub(other Decimal) Decimal {
	return Decimal{raw: d.raw - other.raw}
}

// divRoundNearest computes round(num / den) as a big.Int, rounding halves away
// from zero (matching math.Round semantics). `den` must be non-zero.
func divRoundNearest(num, den *big.Int) *big.Int {
	quo := new(big.Int)
	rem := new(big.Int)
	quo.QuoRem(num, den, rem) // truncated toward zero; rem has sign of num
	if rem.Sign() == 0 {
		return quo
	}
	// Round away from zero when 2*|rem| >= |den|.
	twoRem := new(big.Int).Abs(rem)
	twoRem.Lsh(twoRem, 1)
	absDen := new(big.Int).Abs(den)
	if twoRem.Cmp(absDen) >= 0 {
		// Result sign is sign(num)*sign(den); rem carries sign(num).
		if (num.Sign() < 0) == (den.Sign() < 0) {
			quo.Add(quo, big.NewInt(1))
		} else {
			quo.Sub(quo, big.NewInt(1))
		}
	}
	return quo
}

// Mul returns the product of two decimals: (d.raw * other.raw) / Scale.
// Computed via big.Int so large notionals (price * qty) do not lose precision
// the way the previous float64 implementation did.
func (d Decimal) Mul(other Decimal) Decimal {
	num := new(big.Int).Mul(big.NewInt(d.raw), big.NewInt(other.raw))
	res := divRoundNearest(num, big.NewInt(Scale))
	return Decimal{raw: res.Int64()}
}

// Div returns the quotient of two decimals: (d.raw * Scale) / other.raw.
// Computed via big.Int to avoid float64 rounding error.
func (d Decimal) Div(other Decimal) Decimal {
	num := new(big.Int).Mul(big.NewInt(d.raw), big.NewInt(Scale))
	res := divRoundNearest(num, big.NewInt(other.raw))
	return Decimal{raw: res.Int64()}
}

// Neg returns the negation of the decimal.
func (d Decimal) Neg() Decimal {
	return Decimal{raw: -d.raw}
}

// Cmp compares two decimals. Returns -1 if d < other, 0 if equal, 1 if d > other.
func (d Decimal) Cmp(other Decimal) int {
	switch {
	case d.raw < other.raw:
		return -1
	case d.raw > other.raw:
		return 1
	default:
		return 0
	}
}

// Equal returns true if two decimals have the same value.
func (d Decimal) Equal(other Decimal) bool {
	return d.raw == other.raw
}
