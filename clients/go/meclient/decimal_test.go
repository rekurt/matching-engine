package meclient

import (
	"math"
	"testing"
)

func TestNewDecimal(t *testing.T) {
	tests := []struct {
		name  string
		input float64
		raw   int64
	}{
		{"zero", 0, 0},
		{"one", 1.0, 100_000_000},
		{"negative", -1.0, -100_000_000},
		{"fractional", 1.5, 150_000_000},
		{"small", 0.00000001, 1},
		{"large", 92233720368.0, 9223372036800000000},
		{"negative_frac", -0.123, -12_300_000},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			d := NewDecimal(tt.input)
			if d.Raw() != tt.raw {
				t.Errorf("NewDecimal(%v).Raw() = %d, want %d", tt.input, d.Raw(), tt.raw)
			}
		})
	}
}

func TestDecimalToFloat64(t *testing.T) {
	tests := []struct {
		raw  int64
		want float64
	}{
		{0, 0},
		{100_000_000, 1.0},
		{150_000_000, 1.5},
		{-100_000_000, -1.0},
		{1, 0.00000001},
	}
	for _, tt := range tests {
		d := NewDecimalRaw(tt.raw)
		got := d.ToFloat64()
		if math.Abs(got-tt.want) > 1e-15 {
			t.Errorf("NewDecimalRaw(%d).ToFloat64() = %v, want %v", tt.raw, got, tt.want)
		}
	}
}

func TestDecimalString(t *testing.T) {
	tests := []struct {
		raw  int64
		want string
	}{
		{0, "0.00000000"},
		{100_000_000, "1.00000000"},
		{150_000_000, "1.50000000"},
		{-150_000_000, "-1.50000000"},
		{1, "0.00000001"},
		{12345678, "0.12345678"},
	}
	for _, tt := range tests {
		d := NewDecimalRaw(tt.raw)
		got := d.String()
		if got != tt.want {
			t.Errorf("NewDecimalRaw(%d).String() = %q, want %q", tt.raw, got, tt.want)
		}
	}
}

func TestDecimalIsZero(t *testing.T) {
	if !NewDecimalRaw(0).IsZero() {
		t.Error("expected zero")
	}
	if NewDecimalRaw(1).IsZero() {
		t.Error("expected non-zero")
	}
}

func TestDecimalAdd(t *testing.T) {
	a := NewDecimal(1.5)
	b := NewDecimal(2.3)
	got := a.Add(b)
	want := NewDecimal(3.8)
	if !got.Equal(want) {
		t.Errorf("1.5 + 2.3 = %v, want %v", got, want)
	}
}

func TestDecimalSub(t *testing.T) {
	a := NewDecimal(5.0)
	b := NewDecimal(2.0)
	got := a.Sub(b)
	want := NewDecimal(3.0)
	if !got.Equal(want) {
		t.Errorf("5.0 - 2.0 = %v, want %v", got, want)
	}
}

func TestDecimalMul(t *testing.T) {
	a := NewDecimal(2.0)
	b := NewDecimal(3.5)
	got := a.Mul(b)
	want := NewDecimal(7.0)
	if !got.Equal(want) {
		t.Errorf("2.0 * 3.5 = %v, want %v", got, want)
	}
}

func TestDecimalDiv(t *testing.T) {
	a := NewDecimal(7.0)
	b := NewDecimal(2.0)
	got := a.Div(b)
	want := NewDecimal(3.5)
	if !got.Equal(want) {
		t.Errorf("7.0 / 2.0 = %v, want %v", got, want)
	}
}

func TestDecimalNeg(t *testing.T) {
	d := NewDecimal(1.5)
	got := d.Neg()
	want := NewDecimal(-1.5)
	if !got.Equal(want) {
		t.Errorf("neg(1.5) = %v, want %v", got, want)
	}
}

func TestDecimalCmp(t *testing.T) {
	a := NewDecimal(1.0)
	b := NewDecimal(2.0)
	c := NewDecimal(1.0)

	if a.Cmp(b) != -1 {
		t.Error("1 < 2 should return -1")
	}
	if b.Cmp(a) != 1 {
		t.Error("2 > 1 should return 1")
	}
	if a.Cmp(c) != 0 {
		t.Error("1 == 1 should return 0")
	}
}

func TestDecimalRoundtrip(t *testing.T) {
	values := []float64{0, 1, -1, 0.00000001, 123456.789, -99999.99999999}
	for _, v := range values {
		d := NewDecimal(v)
		got := d.ToFloat64()
		if math.Abs(got-v) > 0.000000005 {
			t.Errorf("roundtrip(%v) = %v", v, got)
		}
	}
}

func TestDecimalFromUint64(t *testing.T) {
	d := NewDecimalFromUint64(100_000_000)
	if d.ToFloat64() != 1.0 {
		t.Errorf("expected 1.0, got %v", d.ToFloat64())
	}
	if d.RawUint64() != 100_000_000 {
		t.Errorf("expected raw 100000000, got %d", d.RawUint64())
	}
}
