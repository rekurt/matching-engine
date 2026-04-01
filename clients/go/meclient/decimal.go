package meclient

import (
	"fmt"
	"math"
)

// Scale is the fixed-point scale factor used by the matching engine (10^8).
const Scale int64 = 100_000_000

// Decimal is a fixed-point decimal with scale 10^8, stored as a raw int64.
// This matches the engine's internal representation for prices and quantities.
type Decimal struct {
	raw int64
}

// NewDecimal creates a Decimal from a float64 value. The float is multiplied
// by Scale and rounded to the nearest integer.
func NewDecimal(f float64) Decimal {
	return Decimal{raw: int64(math.Round(f * float64(Scale)))}
}

// DecimalFromRaw creates a Decimal directly from the raw fixed-point value.
func DecimalFromRaw(raw int64) Decimal {
	return Decimal{raw: raw}
}

// UnsignedDecimalFromRaw creates a Decimal from an unsigned raw value. This is
// used for quantity fields which are transmitted as u64.
func UnsignedDecimalFromRaw(raw uint64) Decimal {
	return Decimal{raw: int64(raw)}
}

// Raw returns the underlying fixed-point integer.
func (d Decimal) Raw() int64 {
	return d.raw
}

// RawUnsigned returns the underlying value cast to uint64. This is used when
// encoding quantity fields.
func (d Decimal) RawUnsigned() uint64 {
	return uint64(d.raw)
}

// ToFloat64 converts the fixed-point Decimal back to a float64.
func (d Decimal) ToFloat64() float64 {
	return float64(d.raw) / float64(Scale)
}

// String returns a human-readable decimal string with up to 8 decimal places,
// trimming trailing zeros.
func (d Decimal) String() string {
	sign := ""
	v := d.raw
	if v < 0 {
		sign = "-"
		v = -v
	}
	whole := v / Scale
	frac := v % Scale
	if frac == 0 {
		return fmt.Sprintf("%s%d", sign, whole)
	}
	fracStr := fmt.Sprintf("%08d", frac)
	// Trim trailing zeros.
	i := len(fracStr) - 1
	for i > 0 && fracStr[i] == '0' {
		i--
	}
	fracStr = fracStr[:i+1]
	return fmt.Sprintf("%s%d.%s", sign, whole, fracStr)
}

// IsZero returns true if the Decimal value is zero.
func (d Decimal) IsZero() bool {
	return d.raw == 0
}

// Add returns d + other.
func (d Decimal) Add(other Decimal) Decimal {
	return Decimal{raw: d.raw + other.raw}
}

// Sub returns d - other.
func (d Decimal) Sub(other Decimal) Decimal {
	return Decimal{raw: d.raw - other.raw}
}

// Mul returns d * other. Because both operands are scaled, the result is
// divided by Scale to maintain the correct magnitude.
func (d Decimal) Mul(other Decimal) Decimal {
	// Use big-style split to avoid overflow for moderate values.
	// For simplicity, use float64 intermediary; this is acceptable for a
	// client library where absolute precision of multiplication is
	// secondary to encode/decode correctness.
	r := float64(d.raw) * float64(other.raw) / float64(Scale)
	return Decimal{raw: int64(math.Round(r))}
}

// Neg returns -d.
func (d Decimal) Neg() Decimal {
	return Decimal{raw: -d.raw}
}

// Cmp compares two Decimals. Returns -1, 0, or 1.
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

// Equal returns true if d == other.
func (d Decimal) Equal(other Decimal) bool {
	return d.raw == other.raw
}
