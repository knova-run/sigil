package sample

import (
	"fmt"
	js "encoding/json"
)

// Base is a struct that the heritage test fixture embeds.
type Base struct {
	ID int
}

// Embedder embeds Base — heritage should record `embed → Base`.
type Embedder struct {
	Base
	Name string
}

// PointerEmbedder embeds *Base — pointer wrapping should still resolve.
type PointerEmbedder struct {
	*Base
	Note string
}

// QualifiedEmbedder embeds an imported type via the aliased import.
// The parser should still recognise this as an embed; the heritage target
// is the literal selector form `js.RawMessage`.
type QualifiedEmbedder struct {
	js.RawMessage
}

// Caller exists only to exercise the call resolver. It calls:
//   * fmt.Println — resolves via the `fmt` import (confidence 0.8)
//   * js.Marshal  — resolves via the `js` alias (confidence 0.8)
//   * Local       — bare same-file identifier (confidence 1.0)
func Caller() {
	fmt.Println("hello")
	_, _ = js.Marshal(struct{}{})
	Local()
}

// Local is the bare-identifier call target.
func Local() {}
