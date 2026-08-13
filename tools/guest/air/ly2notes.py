# bach-air.ly (Mutopia、パブリックドメイン) のフルート声部を読んで、
# PCスピーカー用の音符表 (PIT分周値, BIOSティック数) をNASMのdw行にする。
#
#   python3 ly2notes.py bach-air.ly > notes.inc
#
# 汎用のLilyPondパーサではない。この1曲に出てくる記法だけを相手にする:
# \relative のオクターブ規則、タイ(~)、付点、装飾(acciaccatura/trill)、
# リピートとalternative。リピートは1回だけ通す (デモは1周で十分)。
#
# 時間の単位はBIOSティック (18.2Hz ≒ 55ms)。32分音符=2ティックに固定すると
# 全音価が整数になり (4分=16ティック≒880ms、テンポ68)、丸め誤差が消える。
import re
import sys

PIT_HZ = 1_193_182
STEP = {'c': 0, 'd': 1, 'e': 2, 'f': 3, 'g': 4, 'a': 5, 'b': 6}
SEMI = {'c': 0, 'd': 2, 'e': 4, 'f': 5, 'g': 7, 'a': 9, 'b': 11}
TICKS_PER_32ND = 2

src = open(sys.argv[1], encoding='utf-8').read()

# フルートブロックを波括弧の釣り合いで切り出す
start = src.index("flute = \\relative c' {")
i = src.index('{', start)
depth, j = 0, i
while True:
    if src[j] == '{':
        depth += 1
    elif src[j] == '}':
        depth -= 1
        if depth == 0:
            break
    j += 1
body = src[i + 1:j]
body = re.sub(r'%[^\n]*', '', body)  # コメント

tokens = body.replace('{', ' { ').replace('}', ' } ').split()


def parse_group(pos):
    """posは '{' の次。イベント列と '}' の次の位置を返す"""
    events = []
    while tokens[pos] != '}':
        pos = parse_token(pos, events)
    return events, pos + 1


def skip_group(pos):
    depth = 1
    while depth:
        if tokens[pos] == '{':
            depth += 1
        elif tokens[pos] == '}':
            depth -= 1
        pos += 1
    return pos


def parse_token(pos, events):
    t = tokens[pos]
    if t == '\\repeat':  # \repeat "volta" 2 { ... } → 1回だけ
        assert tokens[pos + 3] == '{'
        inner, pos = parse_group(pos + 4)
        events.extend(inner)
        return pos
    if t == '\\alternative':  # { {1番} {2番} } → 1番だけ
        assert tokens[pos + 1] == '{' and tokens[pos + 2] == '{'
        first, pos = parse_group(pos + 3)
        events.extend(first)
        assert tokens[pos] == '{'
        pos = skip_group(pos + 1)
        assert tokens[pos] == '}'
        return pos + 1
    if t == '\\acciaccatura':  # 装飾音は落とす (55ms格子より細かい)
        assert tokens[pos + 1] == '{'
        return skip_group(pos + 2)
    if t == '\\key':  # 引数の音名 (d) が音符に化けるので一緒に飛ばす
        return pos + 2
    if t.startswith('\\') or t in ('|', '(', ')'):
        # \set Staff.instrumentName = "Flute" / \clef 等の残りの引数は
        # 音符に見えない語なので、下の音符正規表現に落ちて捨てられる
        return pos + 1
    m = re.fullmatch(r"([a-g])(is|es)?!?('+|,+)?(\d+)?(\.?)(\))?(~)?", t.replace('(', ''))
    if not m:
        return pos + 1  # "volta" や 2、4/4 など
    letter, acc, marks, dur, dot, _, tie = m.groups()
    events.append({
        'letter': letter,
        'acc': +1 if acc == 'is' else -1 if acc == 'es' else 0,
        'marks': (marks or ''),
        'dur': int(dur) if dur else None,
        'dot': dot == '.',
        'tie': tie == '~' or tokens[pos + 1:pos + 2] == ['~'],
    })
    return pos + 1


# ブロック本体には外側の波括弧が無いので、仮の括弧で包んでから舐める
tokens = ['{'] + tokens + ['}']
events, _ = parse_group(1)

# \relative c' : 直前の音に一番近いオクターブを選ぶ。' と , はそこからずらす
notes = []  # (midi, 32分音符いくつ分, tie)
prev_step, prev_oct = STEP['c'], 4  # 基準 c' = C4
prev_dur32 = 8
for ev in events:
    step = STEP[ev['letter']]
    octave = prev_oct
    if step - prev_step > 3:
        octave -= 1
    elif step - prev_step < -3:
        octave += 1
    octave += ev['marks'].count("'") - ev['marks'].count(',')
    prev_step, prev_oct = step, octave
    if ev['dur'] is not None:
        d = 32 // ev['dur']
        prev_dur32 = d + d // 2 if ev['dot'] else d
    midi = 12 * (octave + 1) + SEMI[ev['letter']] + ev['acc']
    if notes and notes[-1][2] and notes[-1][0] == midi:  # タイ: 前の音を伸ばす
        m0, d0, _ = notes.pop()
        notes.append((m0, d0 + prev_dur32, ev['tie']))
    else:
        notes.append((midi, prev_dur32, ev['tie']))

print('; ly2notes.py が bach-air.ly から生成した音符表。手で編集しない')
print('; 形式: dw PIT分周値(0=休符), BIOSティック数。0xFFFF で終端')
total = 0
for k, (midi, dur32, _) in enumerate(notes):
    freq = 440.0 * 2 ** ((midi - 69) / 12)
    div = round(PIT_HZ / freq)
    ticks = dur32 * TICKS_PER_32ND
    # 同じ高さの音が続くときだけ1ティックの隙間を空ける (境目を聞かせる)
    nxt = notes[k + 1][0] if k + 1 < len(notes) else None
    if nxt == midi and ticks > 1:
        print(f'\tdw {div}, {ticks - 1}')
        print('\tdw 0, 1')
    else:
        print(f'\tdw {div}, {ticks}')
    total += ticks
print('\tdw 0xFFFF')
print(f'; {len(notes)}音, {total}ティック ≒ {total * 55 // 1000}秒', file=sys.stderr)
