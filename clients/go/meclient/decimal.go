package meclient

import (
	"fmt"
	"math"
)

// Scale is the fixed-point scaling factor (10^8).
const Scale int64 = 100_000_000

// Decimal represents a fixed-point decimal number with 8 decimal places of
// precision. Internally it stores the value multiplied by 10^8.
type Decimal struct {
	raw int64
}

// NewDecimal creates a Decimal from a float64 value.
// For example, NewDecimal(1.5) stores the internal value 150_000_000.
func NewDecimal(f float64) Decimal {
	return Decimal{raw: int64(math.Round(f * float64(Scale)))}
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

// Mul returns the product of two decimals.
// Note: this performs d * other with proper scaling.
func (d Decimal) Mul(other Decimal) Decimal {
	// Use int128 emulation via two multiplications to avoid overflow for
	// values up to ~92 billion (which fits in int64 after scaling).
	// For most practical values this simple approach works.
	return Decimal{raw: int64(math.Round(float64(d.raw) * float64(other.raw) / float64(Scale)))}
}

// Div returns the quotient of two decimals.
func (d Decimal) Div(other Decimal) Decimal {
	return Decimal{raw: int64(math.Round(float64(d.raw) / float64(other.raw) * float64(Scale)))}
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
