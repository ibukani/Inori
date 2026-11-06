"""Validate shipped compositions with no database, network or extension startup."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[1]

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--server',type=Path,default=ROOT/'target/debug'/('inori-server.exe' if os.name=='nt' else 'inori-server'))
    args=p.parse_args()
    (ROOT/'.cache').mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='config-examples-',dir=ROOT/'.cache') as temporary:
        data=Path(temporary)/'not-created'
        for name in ['minimal','autonomous','companion','server','extensions']:
            result=subprocess.run([str(args.server.resolve()),'--data-dir',str(data),'--config',str(ROOT/f'configs/examples/{name}.toml'),'validate-config'],check=True,capture_output=True,text=True,timeout=10)
            graph=json.loads(result.stdout)
            assert graph['valid'] and not data.exists(),name
            if name=='minimal':assert not graph['order']
            if name=='autonomous':assert 'autonomy.standard' in graph['order'] and 'dialogue.standard' not in graph['order']
            print(f'{name}: valid; enabled={len(graph["order"])}; data directory not created')

if __name__=='__main__':main()
