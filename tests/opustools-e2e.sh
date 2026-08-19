#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Usage: tests/opustools-e2e.sh [OPTIONS]

Options:
  --audio FILE          Initial .opus/.ogg/.oga, .wav/.wave, .flac, .pcm, or .raw file
  --photo FILE          Initial PNG, JPEG, GIF, or WebP cover image
  --pcm-rate HZ         Raw PCM sample rate (default: 48000)
  --pcm-channels N      Raw PCM channel count (default: 1)
  --pcm-bits N          Raw PCM bits per sample (default: 16)
  --pcm-endianness N    Raw PCM byte order: 0 little, 1 big (default: 0)
  -h, --help            Show this help

The audio and photo options are independent. When either is omitted, the test
uses its bundled default fixture. The same settings can be supplied as
E2E_AUDIO, E2E_PHOTO, E2E_PCM_RATE, E2E_PCM_CHANNELS, E2E_PCM_BITS, and
E2E_PCM_ENDIANNESS environment variables.
EOF
}

fail() {
    echo "opustools-e2e: $*" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null || {
        echo "missing required tool: $1" >&2
        exit 77
    }
}

assert_contains() {
    grep -F -- "$2" "$1" >/dev/null || fail "expected '$2' in $1"
}

assert_not_contains() {
    if grep -F -- "$2" "$1" >/dev/null; then
        fail "unexpected '$2' in $1"
    fi
}

tag_count() {
    "$binary" show "$1" | sed -n 's/^Tags: //p'
}

validate_with_opusinfo() {
    input=$1
    log=$2
    if opusinfo "$input" >"$log" 2>&1; then
        return
    fi
    webp_count=$("$binary" cover-list "$input" \
        | sed -n '/ mime=image\/webp /p' | wc -l | tr -d ' ')
    unknown_count=$(sed -n '/^WARNING: Unknown media type .*"image\/webp" may not be well-supported$/p' \
        "$log" | wc -l | tr -d ' ')
    mismatch_count=$(sed -n '/^WARNING: Mismatched picture parameters .* \(width\|height\|depth\) declared as .* but appears to be 0$/p' \
        "$log" | wc -l | tr -d ' ')
    issue_count=$(sed -n '/^\(WARNING\|ERROR\):/p' "$log" | wc -l | tr -d ' ')
    if [ "$webp_count" -gt 0 ] \
        && [ "$unknown_count" -eq "$webp_count" ] \
        && [ "$mismatch_count" -eq $((webp_count * 3)) ] \
        && [ "$issue_count" -eq $((webp_count * 4)) ]
    then
        echo "opusinfo emitted only known WebP compatibility warnings"
        return
    fi
    cat "$log" >&2
    fail "opusinfo rejected $input"
}

file_mode() {
    if mode=$(stat -c %a "$1" 2>/dev/null); then
        printf '%s\n' "$mode"
    else
        stat -f %Lp "$1"
    fi
}

assert_audio_unchanged() {
    comment=$1
    input=$2
    audio_check=$((audio_check + 1))
    decoded=$test_dir/audio-check-$audio_check.wav
    if ! opusdec --quiet "$input" "$decoded"; then
        fail "$comment: opusdec could not decode the changed file"
    fi
    if ! cmp "$test_dir/before.wav" "$decoded"; then
        fail "$comment: decoded audio differs from the original"
    fi
    packets=$test_dir/audio-check-$audio_check.packets
    if ! "$binary" audio-dump "$input" "$packets"; then
        fail "$comment: could not extract Opus audio packets"
    fi
    if ! cmp "$test_dir/before.packets" "$packets"; then
        fail "$comment: Opus audio packet bytes or boundaries changed"
    fi
    rm -f "$decoded" "$packets"
    echo "decoded audio and packets unchanged: $comment"
}

audio_source=${E2E_AUDIO:-}
photo_source=${E2E_PHOTO:-}
pcm_rate=${E2E_PCM_RATE:-48000}
pcm_channels=${E2E_PCM_CHANNELS:-1}
pcm_bits=${E2E_PCM_BITS:-16}
pcm_endianness=${E2E_PCM_ENDIANNESS:-0}
audio_check=0
remove_field=E2E_REMOVE_$$

while [ "$#" -gt 0 ]; do
    case $1 in
        --audio|--photo|--pcm-rate|--pcm-channels|--pcm-bits|--pcm-endianness)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            option=$1
            value=$2
            shift 2
            case $option in
                --audio) audio_source=$value ;;
                --photo) photo_source=$value ;;
                --pcm-rate) pcm_rate=$value ;;
                --pcm-channels) pcm_channels=$value ;;
                --pcm-bits) pcm_bits=$value ;;
                --pcm-endianness) pcm_endianness=$value ;;
            esac
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option '$1' (use --help)"
            ;;
    esac
done

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=${OPUSTAGGER_BIN:-"$project_dir/target/debug/opustagger"}
[ -x "$binary" ] || fail "binary is not executable: $binary (run cargo build first)"

for tool in opusdec opusinfo cmp cp grep sed tr wc mktemp ln chmod stat; do
    require_tool "$tool"
done
[ -z "$audio_source" ] || [ -f "$audio_source" ] || fail "audio file not found: $audio_source"
[ -z "$photo_source" ] || [ -f "$photo_source" ] || fail "photo file not found: $photo_source"

test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM

if [ -z "$audio_source" ]; then
    audio_source=$project_dir/tests/sample.wav
    [ -f "$audio_source" ] || fail "bundled audio fixture not found: $audio_source"
    audio_kind=encoded
else
    audio_name=$(basename -- "$audio_source" | tr '[:upper:]' '[:lower:]')
    case $audio_name in
        *.opus|*.ogg|*.oga) audio_kind=opus ;;
        *.wav|*.wave|*.flac) audio_kind=encoded ;;
        *.pcm|*.raw) audio_kind=pcm ;;
        *) fail "unsupported audio extension: $audio_source" ;;
    esac
fi

case $audio_kind in
    opus)
        cp "$audio_source" "$test_dir/original.opus"
        ;;
    encoded)
        require_tool opusenc
        opusenc --quiet --title Original --artist One --artist Two \
            "$audio_source" "$test_dir/original.opus"
        ;;
    pcm)
        require_tool opusenc
        opusenc --quiet --raw --raw-rate "$pcm_rate" --raw-chan "$pcm_channels" \
            --raw-bits "$pcm_bits" --raw-endianness "$pcm_endianness" \
            --title Original --artist One --artist Two \
            "$audio_source" "$test_dir/original.opus"
        ;;
esac

if [ -z "$photo_source" ]; then
    photo_source=$project_dir/tests/cover.png
fi

validate_with_opusinfo "$test_dir/original.opus" "$test_dir/original-info"
opusdec --quiet "$test_dir/original.opus" "$test_dir/before.wav"
"$binary" audio-dump "$test_dir/original.opus" "$test_dir/before.packets"

# Separate output, case-insensitive replacement, UTF-8, and '=' in a value.
cp "$test_dir/original.opus" "$test_dir/original-before-output.opus"
cp "$test_dir/original.opus" "$test_dir/tagged.opus"
chmod 2600 "$test_dir/tagged.opus"
"$binary" set "$test_dir/original.opus" TITLE "Edited = 東京" \
    --output "$test_dir/tagged.opus"
cmp "$test_dir/original-before-output.opus" "$test_dir/original.opus" || \
    fail "separate-output edit changed its input file"
[ "$(file_mode "$test_dir/tagged.opus")" = 2600 ] || \
    fail "separate-output edit weakened existing output permissions"
assert_audio_unchanged "set TITLE using separate output" "$test_dir/tagged.opus"

# In-place edits follow an input symlink and preserve the link itself.
cp "$test_dir/tagged.opus" "$test_dir/symlink-target.opus"
ln -s symlink-target.opus "$test_dir/symlink-input.opus"
"$binary" set "$test_dir/symlink-input.opus" E2E_SYMLINK followed
[ -L "$test_dir/symlink-input.opus" ] || fail "in-place edit replaced its input symlink"
"$binary" show "$test_dir/symlink-target.opus" >"$test_dir/symlink-show"
assert_contains "$test_dir/symlink-show" "E2E_SYMLINK=followed"
assert_audio_unchanged "edit through input symlink" "$test_dir/symlink-target.opus"

# In-place vendor, repeated-field, exact-index edit, and exact-index removal.
"$binary" vendor "$test_dir/tagged.opus" "opustagger e2e vendor"
assert_audio_unchanged "edit vendor" "$test_dir/tagged.opus"
"$binary" add "$test_dir/tagged.opus" E2E_REPEAT First
assert_audio_unchanged "add first repeated tag" "$test_dir/tagged.opus"
"$binary" add "$test_dir/tagged.opus" e2e_repeat Second
assert_audio_unchanged "add second repeated tag" "$test_dir/tagged.opus"
"$binary" set "$test_dir/tagged.opus" E2E_REPEAT "Consolidated = value"
assert_audio_unchanged "consolidate repeated tags" "$test_dir/tagged.opus"
"$binary" set "$test_dir/tagged.opus" E2E_OPTION -- -o
assert_audio_unchanged "set a literal -o tag value" "$test_dir/tagged.opus"
control_value=$(printf 'line 1\nline 2\033[31m')
"$binary" add "$test_dir/tagged.opus" E2E_CONTROL "$control_value"
assert_audio_unchanged "add control characters for safe display" "$test_dir/tagged.opus"
"$binary" add "$test_dir/tagged.opus" E2E_EDIT before
assert_audio_unchanged "add tag for indexed editing" "$test_dir/tagged.opus"
edit_index=$(tag_count "$test_dir/tagged.opus")
[ -n "$edit_index" ] || fail "could not read tag count"
edit_index=$((edit_index - 1))
"$binary" edit "$test_dir/tagged.opus" "$edit_index" E2E_EDIT "after = ✓"
assert_audio_unchanged "edit tag by index" "$test_dir/tagged.opus"
"$binary" add "$test_dir/tagged.opus" "$remove_field" transient
assert_audio_unchanged "add tag for indexed removal" "$test_dir/tagged.opus"
remove_index=$(tag_count "$test_dir/tagged.opus")
[ -n "$remove_index" ] || fail "could not read tag count"
remove_index=$((remove_index - 1))
"$binary" remove "$test_dir/tagged.opus" "$remove_index"
assert_audio_unchanged "remove tag by index" "$test_dir/tagged.opus"

"$binary" show "$test_dir/tagged.opus" >"$test_dir/show"
assert_contains "$test_dir/show" "Vendor: opustagger e2e vendor"
assert_contains "$test_dir/show" "TITLE=Edited = 東京"
assert_contains "$test_dir/show" "E2E_REPEAT=Consolidated = value"
assert_contains "$test_dir/show" "E2E_OPTION=-o"
assert_contains "$test_dir/show" 'E2E_CONTROL=line 1\nline 2\u{1b}[31m'
assert_contains "$test_dir/show" "E2E_EDIT=after = ✓"
assert_not_contains "$test_dir/show" "$remove_field="
repeat_count=$(grep -Fc -- "E2E_REPEAT=" "$test_dir/show")
[ "$repeat_count" -eq 1 ] || fail "case-insensitive set did not consolidate repeated fields"

# Multiple embedded pictures, extraction, list navigation, and reindexing after removal.
initial_cover_count=$(
    "$binary" cover-list "$test_dir/tagged.opus" | sed -n 's/^Covers: //p'
)
[ -n "$initial_cover_count" ] || fail "could not read initial cover count"
"$binary" cover-add "$test_dir/tagged.opus" "$photo_source" "Front = cover"
assert_audio_unchanged "add first cover" "$test_dir/tagged.opus"
"$binary" cover-add "$test_dir/tagged.opus" "$photo_source" "Second cover"
assert_audio_unchanged "add second cover" "$test_dir/tagged.opus"
"$binary" cover-list "$test_dir/tagged.opus" >"$test_dir/covers"
first_added_cover=$initial_cover_count
second_added_cover=$((initial_cover_count + 1))
assert_contains "$test_dir/covers" "[$first_added_cover]"
assert_contains "$test_dir/covers" "[$second_added_cover]"
assert_contains "$test_dir/covers" "Covers: $((initial_cover_count + 2))"
"$binary" cover-extract "$test_dir/tagged.opus" "$first_added_cover" "$test_dir/extracted-cover-0"
cmp "$photo_source" "$test_dir/extracted-cover-0"
"$binary" cover-remove "$test_dir/tagged.opus" "$first_added_cover"
assert_audio_unchanged "remove cover by index" "$test_dir/tagged.opus"
"$binary" cover-list "$test_dir/tagged.opus" >"$test_dir/covers-after-remove"
assert_contains "$test_dir/covers-after-remove" "Covers: $((initial_cover_count + 1))"
"$binary" cover-extract "$test_dir/tagged.opus" "$first_added_cover" "$test_dir/extracted-cover-1"
cmp "$photo_source" "$test_dir/extracted-cover-1"

# Malformed picture comments remain numbered and do not hide later valid covers.
"$binary" add "$test_dir/tagged.opus" METADATA_BLOCK_PICTURE invalid-base64
assert_audio_unchanged "add malformed picture comment" "$test_dir/tagged.opus"
"$binary" cover-add "$test_dir/tagged.opus" "$photo_source" "Cover after invalid entry"
assert_audio_unchanged "add cover after malformed picture comment" "$test_dir/tagged.opus"
"$binary" cover-list "$test_dir/tagged.opus" >"$test_dir/covers-with-invalid"
invalid_cover=$((initial_cover_count + 1))
cover_after_invalid=$((initial_cover_count + 2))
assert_contains "$test_dir/covers-with-invalid" "[$invalid_cover]"
assert_contains "$test_dir/covers-with-invalid" "invalid="
assert_contains "$test_dir/covers-with-invalid" "[$cover_after_invalid]"
assert_contains "$test_dir/covers-with-invalid" "Covers: $((initial_cover_count + 3))"
"$binary" cover-extract "$test_dir/tagged.opus" "$cover_after_invalid" "$test_dir/extracted-after-invalid"
cmp "$photo_source" "$test_dir/extracted-after-invalid"
"$binary" cover-remove "$test_dir/tagged.opus" "$invalid_cover"
assert_audio_unchanged "remove malformed picture by cover index" "$test_dir/tagged.opus"

# A large value forces OpusTags to span several Ogg pages.
require_tool dd
large_value=$(dd if=/dev/zero bs=70000 count=1 2>/dev/null | tr '\000' x)
"$binary" add "$test_dir/tagged.opus" E2E_LARGE "$large_value"
assert_audio_unchanged "add multi-page tag" "$test_dir/tagged.opus"

# Invalid image and index operations must fail without changing the file.
cp "$test_dir/tagged.opus" "$test_dir/before-invalid.opus"
printf 'not an image' >"$test_dir/invalid-cover"
if "$binary" cover-add "$test_dir/tagged.opus" "$test_dir/invalid-cover" 2>/dev/null; then
    fail "invalid cover was accepted"
fi
cmp "$test_dir/before-invalid.opus" "$test_dir/tagged.opus"
assert_audio_unchanged "reject invalid cover" "$test_dir/tagged.opus"
if "$binary" remove "$test_dir/tagged.opus" 999999 2>/dev/null; then
    fail "invalid tag index was accepted"
fi
cmp "$test_dir/before-invalid.opus" "$test_dir/tagged.opus"
assert_audio_unchanged "reject invalid tag index" "$test_dir/tagged.opus"
if "$binary" audio-dump "$test_dir/tagged.opus" "$test_dir/tagged.opus" 2>/dev/null; then
    fail "audio-dump accepted its input as its output"
fi
cmp "$test_dir/before-invalid.opus" "$test_dir/tagged.opus"
assert_audio_unchanged "reject audio-dump output alias" "$test_dir/tagged.opus"
if "$binary" cover-extract "$test_dir/tagged.opus" 0 "$test_dir/tagged.opus" 2>/dev/null; then
    fail "cover-extract accepted its input as its output"
fi
cmp "$test_dir/before-invalid.opus" "$test_dir/tagged.opus"
assert_audio_unchanged "reject cover-extract output alias" "$test_dir/tagged.opus"

# Final independent container validation.
validate_with_opusinfo "$test_dir/tagged.opus" "$test_dir/tagged-info"

echo "opus-tools end-to-end check passed ($audio_kind audio, $audio_check audio checks)"
