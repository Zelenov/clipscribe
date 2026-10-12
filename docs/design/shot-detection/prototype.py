"""Prototype shot detection on 64x36 grey frames. Usage: prototype.py luma.raw fps"""
import sys
W, H = 64, 36
data = open(sys.argv[1], "rb").read(); fps = float(sys.argv[2])
n = len(data) // (W * H)
F = [data[i*W*H:(i+1)*W*H] for i in range(n)]
def mad(a, b): return sum(abs(x - y) for x, y in zip(a, b)) * 100.0 / (len(a) * 255)
def mean(a): return sum(a) * 100.0 / (len(a) * 255)
def cells(a, s=8):  # mean luma of an s x s grid
    cw, ch = W // s, H // s
    return [sum(a[(y*ch+j)*W + x*cw + i] for j in range(ch) for i in range(cw)) / (cw*ch)
            for y in range(s) for x in range(s)]
def uniform_blend(a, m, b):
    """A dissolve or fade moves most of the frame the same fraction of the way from a to b.
    A person walking in changes one region. True when >= 60 % of cells moved, consistently."""
    ca, cm, cb = cells(a), cells(m), cells(b)
    ws = [(y - x) / (z - x) for x, y, z in zip(ca, cm, cb) if abs(z - x) >= 6]
    if len(ws) < 0.6 * len(ca): return False
    ws.sort(); mid = ws[len(ws)//2]
    spread = ws[int(len(ws)*0.8)] - ws[int(len(ws)*0.2)]
    return 0.15 <= mid <= 0.85 and spread <= 0.3

HARD = 10.0          # FFmpeg scdet's default: always a cut
SOFT = 0.5           # a weaker spike is a cut only when isolated (same-scene cuts)
MIN_SHOT = int(fps * 0.5)
d = [0.0] + [mad(F[i-1], F[i]) for i in range(1, n)]
sc = [0.0, 0.0] + [min(d[i], abs(d[i] - d[i-1])) for i in range(2, n)]  # FFmpeg scdet score
WIN = int(fps)
cand = []
for i in range(2, n):
    if sc[i] >= HARD:
        cand.append(i); continue
    # isolated: well above every other score within 1 s, ignoring i±1 and other hard cuts
    nb = [sc[j] for j in range(max(2, i-WIN), min(n, i+WIN+1)) if abs(j - i) > 1 and sc[j] < HARD]
    if sc[i] >= SOFT and sc[i] >= 4.0 * max(max(nb, default=0.0), 0.05):
        cand.append(i)
cuts, flashes, skip = [], [], set()
for i in cand:
    if i in skip: continue
    back = [(mad(F[i-1], F[j]), j) for j in range(i+1, min(n, i+5))]
    m = min(back) if back else (99, i)
    if m[0] < 0.35 * d[i]:            # the picture comes back: a flash
        flashes.append(i); skip.update(range(i, m[1] + 1)); continue
    if cuts and i - cuts[-1] < MIN_SHOT:  # keep the stronger of two cuts too close together
        if sc[i] > sc[cuts[-1]]: cuts[-1] = i
        continue
    cuts.append(i)
G = int(fps * 0.5)
grads, i = [], G
while i < n:
    a, b = i - G, i
    span, steps = mad(F[a], F[b]), max(d[a+1:b+1])
    if span >= 5.0 and steps < 0.35 * span and not any(a - 2 <= c <= b + 2 for c in cuts if sc[c] >= HARD) \
       and not any(a + 2 <= c <= b - 2 for c in cuts) \
       and uniform_blend(F[a], F[(a+b)//2], F[b]):
        while b + 1 < n and mad(F[a], F[b+1]) > mad(F[a], F[b]) + 0.2 and d[b+1] < 0.35 * mad(F[a], F[b+1]):
            b += 1
        kind = "fade to black" if mean(F[b]) < 4 else ("fade from black" if mean(F[a]) < 4 else "dissolve")
        grads.append((a, b, kind)); i = b + G
    else:
        i += 1
# a weak "cut" at the very start of a gradual transition is its first step: merge it in
cuts = [c for c in cuts if sc[c] >= HARD or not any(a - G <= c <= b for a, b, _ in grads)]
t = lambda i: f"{i/fps:.2f}"
print("cuts:", [t(i) for i in cuts])
print("flashes ignored:", [t(i) for i in flashes])
print("gradual:", [(t(a), t(b), k) for a, b, k in grads])
bounds = sorted(set([0] + cuts + [a for a, _, _ in grads] + [n]))
print("shots:", len(bounds) - 1, [(t(a), t(b)) for a, b in zip(bounds, bounds[1:])])
