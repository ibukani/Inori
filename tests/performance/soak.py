"""Bounded liveness sampler. This measures HTTPS health, NOT conversational latency."""
import argparse
from collections import deque
import json
from pathlib import Path
import ssl
import statistics
import time
import urllib.request

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--url",required=True)
    parser.add_argument("--certificate",required=True,type=Path)
    parser.add_argument("--seconds",type=int,default=60)
    parser.add_argument("--output",type=Path,default=Path("artifacts/performance/soak.json"))
    args=parser.parse_args()
    if not args.url.startswith("https://") or not 1 <= args.seconds <= 86400:
        parser.error("explicit HTTPS and a duration from 1 to 86400 seconds are required")
    context=ssl.create_default_context(cafile=str(args.certificate))
    samples=deque(maxlen=4096); failures=0; count=0
    started=time.monotonic(); interrupted=False
    try:
        while time.monotonic()-started<args.seconds:
            at=time.perf_counter()
            try:
                with urllib.request.urlopen(args.url.rstrip('/')+'/healthz',context=context,timeout=5) as response:
                    assert json.load(response)["status"]=="ok"
                samples.append((time.perf_counter()-at)*1000)
            except Exception: failures+=1
            count+=1
            time.sleep(min(1,max(0,args.seconds-(time.monotonic()-started))))
    except KeyboardInterrupt: interrupted=True
    ordered=sorted(samples)
    report={"kind":"HTTPS health liveness, not speech latency","requested_seconds":args.seconds,"elapsed_seconds":round(time.monotonic()-started,2),"attempts":count,"failures":failures,"interrupted":interrupted,"latency_window":len(samples),"recent_p50_ms":round(statistics.median(samples),2) if samples else None,"recent_p95_ms":round(ordered[min(len(ordered)-1,int(len(ordered)*.95))],2) if ordered else None}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(report,ensure_ascii=False))
    if failures or interrupted: raise SystemExit(1)

if __name__=="__main__": main()
