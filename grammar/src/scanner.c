#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stddef.h>

/* External tokens for Unreal Angelscript format strings (f"..."):
 *
 *   FORMAT_STRING_CONTENT  literal text chunk inside an f-string.  Stops
 *                          before an interpolation '{' or the closing '"'.
 *                          '{{' / '}}' are literal braces and stay inside
 *                          the chunk.  Handled by the scanner instead of a
 *                          regex so that whitespace and '//' sequences are
 *                          kept verbatim (grammar `extras` would otherwise
 *                          eat them).
 *   FORMAT_SPEC            python-style format spec after ':' inside an
 *                          interpolation, e.g. `{Value :#032b}`.
 *
 * Everything else in the language is lexable with plain tree-sitter tokens.
 */
enum TokenType {
  FORMAT_STRING_CONTENT,
  FORMAT_SPEC,
};

void *tree_sitter_angelscript_external_scanner_create(void) { return NULL; }

void tree_sitter_angelscript_external_scanner_destroy(void *payload) {
  (void)payload;
}

unsigned tree_sitter_angelscript_external_scanner_serialize(void *payload,
                                                            char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_angelscript_external_scanner_deserialize(void *payload,
                                                          const char *buffer,
                                                          unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

static bool scan_format_string_content(TSLexer *lexer) {
  bool consumed = false;

  lexer->mark_end(lexer);

  for (;;) {
    int32_t c = lexer->lookahead;

    if (lexer->eof(lexer) || c == '"' || c == '\n' || c == '\r') {
      break;
    }

    if (c == '{') {
      /* Either an interpolation start (stop here) or an escaped '{{'. */
      advance(lexer);
      if (lexer->lookahead != '{') {
        break;
      }
      advance(lexer);
      consumed = true;
      lexer->mark_end(lexer);
      continue;
    }

    if (c == '\\') {
      advance(lexer);
      if (!lexer->eof(lexer)) {
        advance(lexer);
      }
      consumed = true;
      lexer->mark_end(lexer);
      continue;
    }

    advance(lexer);
    consumed = true;
    lexer->mark_end(lexer);
  }

  if (!consumed) {
    return false;
  }

  lexer->result_symbol = FORMAT_STRING_CONTENT;
  return true;
}

static bool scan_format_spec(TSLexer *lexer) {
  bool consumed = false;

  while (!lexer->eof(lexer)) {
    int32_t c = lexer->lookahead;
    if (c == '}' || c == '{' || c == '"' || c == '\n' || c == '\r') {
      break;
    }
    advance(lexer);
    consumed = true;
  }

  if (!consumed) {
    return false;
  }

  lexer->mark_end(lexer);
  lexer->result_symbol = FORMAT_SPEC;
  return true;
}

bool tree_sitter_angelscript_external_scanner_scan(void *payload,
                                                   TSLexer *lexer,
                                                   const bool *valid_symbols) {
  (void)payload;

  /* In error recovery every external token is marked valid; bail out so the
   * scanner never swallows arbitrary source text. */
  if (valid_symbols[FORMAT_STRING_CONTENT] && valid_symbols[FORMAT_SPEC]) {
    return false;
  }

  if (valid_symbols[FORMAT_SPEC]) {
    return scan_format_spec(lexer);
  }

  if (valid_symbols[FORMAT_STRING_CONTENT]) {
    return scan_format_string_content(lexer);
  }

  return false;
}
