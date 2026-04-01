package meclient

import (
	"math"
	"testing"
)

func TestNewDecimalFromFloat(t *testing.T) {
	tests := []struct {
		f    float64
		want int64
	}{
		{0.0, 0},
		{1.0, 100_000_000},
		{-1.0, -100_000_000},
		{0.00000001, 1},
		{123.456, 12345600000},
		{99999.99999999, 9999999999999},
	}
	for _, tt := range tests {
		d := NewDecimal(tt.f)
		if d.Raw() != tt.want {
			t.Errorf("NewDecimal(%v).Raw() = %d, want %d", tt.f, d.Raw(), tt.want)
		}
	}
}

func TestDecimalToFloat64(t *testing.T) {
	d := DecimalFromRaw(12345600000)
	got := d.ToFloat64()
	if math.Abs(got-123.456) > 1e-9 {
		t.Errorf("ToFloat64() = %v, want 123.456", got)
	}
}

func TestDecimalString(t *testing.T) {
	tests := []struct {
		raw  int64
		want string
	}{
		{0, "0"},
		{100_000_000, "1"},
		{-100_000_000, "-1"},
		{12345600000, "123.456"},
		{1, "0.00000001"},
		{-1, "-0.00000001"},
		{150_000_000, "1.5"},
	}
	for _, tt := range tests {
		d := DecimalFromRaw(tt.raw)
		if got := d.String(); got != tt.want {
			t.Errorf("DecimalFromRaw(%d).String() = %q, want %q", tt.raw, got, tt.want)
		}
	}
}

func TestDecimalAdd(t *testing.T) {
	a := NewDecimal(1.5)
	b := NewDecimal(2.3)
	got := a.Add(b).ToFloat64()
	if math.Abs(got-3.8) > 1e-9 {
		t.Errorf("Add: got %v, want 3.8", got)
	}
}

func TestDecimalSub(t *testing.T) {
	a := NewDecimal(5.0)
	b := NewDecimal(3.0)
	got := a.Sub(b).ToFloat64()
	if math.Abs(got-2.0) > 1e-9 {
		t.Errorf("Sub: got %v, want 2.0", got)
	}
}

func TestDecimalMul(t *testing.T) {
	a := NewDecimal(2.0)
	b := NewDecimal(3.5)
	got := a.Mul(b).ToFloat64()
	if math.Abs(got-7.0) > 1e-9 {
		t.Errorf("Mul: got %v, want 7.0", got)
	}
}

func TestDecimalNeg(t *testing.T) {
	d := NewDecimal(42.0)
	neg := d.Neg()
	if neg.Raw() != -d.Raw() {
		t.Errorf("Neg: got %d, want %d", neg.Raw(), -d.Raw())
	}
}

func TestDecimalCmp(t *testing.T) {
	a := NewDecimal(1.0)
	b := NewDecimal(2.0)
	if a.Cmp(b) != -1 {
		t.Error("1 < 2 should return -1")
	}
	if b.Cmp(a) != 1 {
		t.Error("2 > 1 should return 1")
	}
	if a.Cmp(a) != 0 {
		t.Error("1 == 1 should return 0")
	}
}

func TestDecimalEqual(t *testing.T) {
	a := NewDecimal(1.0)
	b := DecimalFromRaw(100_000_000)
	if !a.Equal(b) {
		t.Error("should be equal")
	}
}

func TestDecimalIsZero(t *testing.T) {
	if !NewDecimal(0).IsZero() {
		t.Error("zero should be zero")
	}
	if NewDecimal(1).IsZero() {
		t.Error("1 should not be zero")
	}
}

func TestUnsignedDecimalFromRaw(t *testing.T) {
	d := UnsignedDecimalFromRaw(100_000_000)
	if d.ToFloat64() != 1.0 {
		t.Errorf("got %v, want 1.0", d.ToFloat64())
	}
}

func TestDecimalRawUnsigned(t *testing.T) {
	d := NewDecimal(1.0)
	if d.RawUnsigned() != 100_000_000 {
		t.Errorf("got %d, want 100000000", d.RawUnsigned())
	}
}

func TestDecimalEdgeCases(t *testing.T) {
	// Max int64 raw
	d := DecimalFromRaw(math.MaxInt64)
	if d.Raw() != math.MaxInt64 {
		t.Error("max int64 raw mismatch")
	}

	// Min int64 raw
	d2 := DecimalFromRaw(math.MinInt64)
	if d2.Raw() != math.MinInt64 {
		t.Error("min int64 raw mismatch")
	}
}
