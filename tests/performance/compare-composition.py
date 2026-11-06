"""Compare two explicit Windows binaries with mock engines and synthetic fresh data.

This measures headless/debug orchestration, not AI or audio latency. Does not use
the owner's data, installed engines, microphone, desktop UI or downloaded models.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import ssl
import statistics
import subprocess
import tempfile
import time
import urllib.request

ROOT=Path(__file__).resolve().parents[2]
FLAGS=subprocess.CREATE_NO_WINDOW if os.name=="nt" else 0

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--before",required=True,type=Path)
    p.add_argument("--after",required=True,type=Path)
    p.add_argument("--client",type=Path,default=ROOT/'target/debug/examples/text-latency.exe')
    p.add_argument("--output",type=Path,default=ROOT/'.cache/composition-performance.json')
    args=p.parse_args()
    if os.name!='nt':p.error('Windows process counters are required by this harness')
    (ROOT/'.cache').mkdir(exist_ok=True)
    results={}
    with tempfile.TemporaryDirectory(prefix='composition-compare-',dir=ROOT/'.cache') as temporary:
        work=Path(temporary)
        for name,binary in [('before',args.before),('after',args.after)]:
            data=work/name;data.mkdir()
            configuration=data/'server.toml'
            common='[autonomy]\nenabled=false\ninterval_seconds=1800\nmax_actions=3\ncloud_enabled=false\ndaily_inference_limit=0\n'
            composition=(ROOT/'configs/examples/companion.toml').read_text(encoding='utf8') if name=='after' else ''
            configuration.write_text(common+composition,encoding='utf8')
            with socket.socket() as probe:probe.bind(('127.0.0.1',0));port=probe.getsockname()[1]
            process=None;meter=None
            with (data/'server.log').open('wb') as log:
                try:
                    process=subprocess.Popen([str(binary.resolve()),'--data-dir',str(data),'--config',str(configuration),'--listen',f'127.0.0.1:{port}'],stdout=log,stderr=log,creationflags=FLAGS)
                    deadline=time.monotonic()+20
                    while True:
                        if process.poll() is not None:raise RuntimeError((data/'server.log').read_text(errors='replace'))
                        try:
                            context=ssl.create_default_context(cafile=str(data/'server.pem'))
                            with urllib.request.urlopen(f'https://127.0.0.1:{port}/healthz',context=context,timeout=2) as r:assert json.load(r)['status']=='ok'
                            break
                        except (OSError,ValueError):
                            if time.monotonic()>deadline:raise
                            time.sleep(.1)
                    client=data/'client.toml'
                    client.write_text('server_url='+json.dumps(f'https://127.0.0.1:{port}')+'\ntoken_file='+json.dumps((data/'owner.token').as_posix())+'\ncertificate_file='+json.dumps((data/'server.pem').as_posix())+'\n',encoding='utf8')
                    subprocess.run([str(args.client.resolve()),str(client),'1'],check=True,capture_output=True,timeout=20,creationflags=FLAGS)
                    sample=data/'resources.json'
                    meter=subprocess.Popen(['pwsh','-NoProfile','-File',str(ROOT/'tests/performance/measure-windows.ps1'),'-ProcessIds',str(process.pid),'-Seconds','20','-Phase','background','-Output',str(sample)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,creationflags=FLAGS)
                    measured=subprocess.run([str(args.client.resolve()),str(client),'20'],check=True,capture_output=True,text=True,timeout=30,creationflags=FLAGS)
                    latency=json.loads(measured.stdout)['samples']
                    _,errors=meter.communicate(timeout=35)
                    if meter.returncode:raise RuntimeError(errors.decode(errors='replace'))
                    counters=json.loads(sample.read_text(encoding='utf-8-sig'))
                    records=counters['Samples'];first=sorted(row['first_text_ms'] for row in latency)
                    results[name]={'sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'sample_seconds':counters['ActualSeconds'],'resource_samples':len(records),'engine':'in-process deterministic mock; no external engine','profile':'debug','warm_text_samples':len(latency),'first_text_p50_ms':statistics.median(first),'first_text_p95_ms':first[18],'full_text_p50_ms':statistics.median(row['full_text_ms'] for row in latency),'working_set_max_mib':max(row['WorkingSetMB'] for row in records),'private_bytes_max_mib':max(row['PrivateBytesMB'] for row in records),'mean_cpu_percent_one_core':round(statistics.mean(row['CpuPercentOfOneCore'] for row in records if row['CpuPercentOfOneCore'] is not None),3),'gpu_counters_available':counters['GpuCountersAvailable']}
                finally:
                    if meter is not None and meter.poll() is None:meter.terminate();meter.wait(timeout=5)
                    if process is not None and process.poll() is None:process.terminate();process.wait(timeout=5)
    report={'kind':'Windows headless mock orchestration comparison; excludes real AI, STT/TTS, playback and GUI','runs':results}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',encoding='utf8')
    print(json.dumps(report,ensure_ascii=False))

if __name__=='__main__':main()
