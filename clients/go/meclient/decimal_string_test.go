package meclient

import "testing"

// C-4: monetary values must be parsed exactly, without a float64 intermediate.
func TestNewDecimalFromStringExact(t *testing.T) {
	cases := []struct {
		in  string
		raw int64
	}{
		{"0", 0},
		{"1.5", 150_000_000},
		{"0.07", 7_000_000}, // not exactly representable as float64*1e8
		{"12345.67", 1_234_567_000_000},
		{"0.00000001", 1},
		{"-0.00000001", -1},
		{"+42", 4_200_000_000},
		{"  3.14  ", 314_000_000},
	}
	for _, c := range cases {
		d, err := NewDecimalFromString(c.in)
		if err != nil {
			t.Fatalf("NewDecimalFromString(%q) error: %v", c.in, err)
		}
		if d.Raw() != c.raw {
			t.Errorf("NewDecimalFromString(%q).Raw() = %d, want %d", c.in, d.Raw(), c.raw)
		}
	}
}

func TestNewDecimalFromStringErrors(t *testing.T) {
	bad := []string{"0.000000001", "abc", "", "1.2.3", "--1"}
	for _, s := range bad {
		if _, err := NewDecimalFromString(s); err == nil {
			t.Errorf("NewDecimalFromString(%q) expected error, got nil", s)
		}
	}
}

// Mul must not lose precision on large notionals the way the old float64 path
// did. price 12345.67 * qty 3.0 = 37037.01 exactly.
func TestMulExactNotional(t *testing.T) {
	price, _ := NewDecimalFromString("12345.67")
	qty, _ := NewDecimalFromString("3.0")
	got := price.Mul(qty)
	want, _ := NewDecimalFromString("37037.01")
	if !got.Equal(want) {
		t.Errorf("Mul = %s (raw %d), want %s (raw %d)", got, got.Raw(), want, want.Raw())
	}
}

func TestMulRoundsHalfAwayFromZero(t *testing.T) {
	// 0.00000001 * 0.5 = 0.000000005 → rounds to 0.00000001 (1 raw), half away.
	a := NewDecimalRaw(1)
	half, _ := NewDecimalFromString("0.5")
	if got := a.Mul(half); got.Raw() != 1 {
		t.Errorf("Mul half-away = %d, want 1", got.Raw())
	}
}

func TestDivExact(t *testing.T) {
	// 1.0 / 4.0 = 0.25
	a, _ := NewDecimalFromString("1.0")
	b, _ := NewDecimalFromString("4.0")
	got := a.Div(b)
	want, _ := NewDecimalFromString("0.25")
	if !got.Equal(want) {
		t.Errorf("Div = %s, want %s", got, want)
	}
}

// Round-trip: parsing the String() output reproduces the same raw value.
func TestStringRoundTripExact(t *testing.T) {
	for _, s := range []string{"12345.67", "0.07", "0.00000001", "-9999.99999999"} {
		d, err := NewDecimalFromString(s)
		if err != nil {
			t.Fatalf("parse %q: %v", s, err)
		}
		back, err := NewDecimalFromString(d.String())
		if err != nil {
			t.Fatalf("reparse %q: %v", d.String(), err)
		}
		if !back.Equal(d) {
			t.Errorf("round-trip %q: got raw %d, want %d", s, back.Raw(), d.Raw())
		}
	}
}
