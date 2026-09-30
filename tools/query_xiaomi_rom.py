#!/usr/bin/env python3
import argparse, base64, json
import requests
from Crypto.Cipher import AES

API_CN='https://update.miui.com/updates/miotaV3.php'
DEVICE_LIST='https://raw.githubusercontent.com/YuKongA/Updater-KMP/device-list/device.json'
KEY=b'miuiotavalided11'; IV=b'0102030405060708'
BASES={'official1':'https://ultimateota.d.miui.com','official2':'https://superota.d.miui.com','cdn1':'https://bkt-sgp-miui-ota-update-alisgp.oss-ap-southeast-1.aliyuncs.com','cdn2':'https://cdnorg.d.miui.com'}

def enc(b):
    n=16-len(b)%16
    return base64.urlsafe_b64encode(AES.new(KEY,AES.MODE_CBC,IV).encrypt(b+bytes([n])*n)).decode()

def dec(s):
    b=base64.b64decode(s); out=AES.new(KEY,AES.MODE_CBC,IV).decrypt(b); return json.loads(out[:-out[-1]].decode())

def get_code(device):
    data=requests.get(DEVICE_LIST,timeout=30).json()
    for x in data.get('devices',[]):
        if x.get('deviceCodeName')==device or x.get('deviceName')==device: return x['deviceCode']
    raise SystemExit('device not found: '+device)

def main():
    p=argparse.ArgumentParser(); p.add_argument('--device',required=True); p.add_argument('--os',required=True); p.add_argument('--android',default='15.0'); p.add_argument('--region',default='CN'); p.add_argument('--carrier',default='XM'); p.add_argument('--api',default=API_CN); a=p.parse_args()
    code=get_code(a.device); region=a.region.upper(); carrier=a.carrier.upper(); letter={'15.0':'V','16.0':'W','17.0':'X'}.get(a.android)
    if not letter: raise SystemExit('unsupported Android version: '+a.android)
    dc=letter+code+region+carrier; system=a.os.upper().replace('.AUTO',dc); branch='X' if system.endswith('.DEV') else 'F'; d=a.device
    q={'b':branch,'c':a.android,'d':d,'f':'1','id':'','l':'zh_CN' if region=='CN' else 'en_US','ov':system,'p':d,'pn':d,'r':region,'security':'','token':'','unlock':'0','v':'MIUI-'+system}
    if float(a.android)>=15: q['options']={'av':'9.3.7'}
    r=requests.post(a.api,data={'q':enc(json.dumps(q,ensure_ascii=False,separators=(',',':')).encode()),'t':'','s':'1'},timeout=45); r.raise_for_status(); obj=dec(r.text)
    result={'query':{'device':d,'deviceCode':code,'os':system,'android':a.android,'region':region,'carrier':carrier},'source':a.api,'roms':[]}
    for slot in ('CurrentRom','LatestRom','IncrementRom','CrossRom'):
        rom=obj.get(slot)
        if not isinstance(rom,dict) or not rom.get('filename'): continue
        path='/'+str(rom.get('version') or rom.get('name') or '')+'/'+rom['filename']
        item={'slot':slot}
        for k in ('type','device','name','version','codebase','branch','filename','filesize','md5','bigversion','osbigversion','isBeta','isGov'): item[k]=rom.get(k)
        item['urls']={k:v+path for k,v in BASES.items()}; result['roms'].append(item)
    result['api_meta']={k:obj.get(k) for k in ('AuthResult','TraceId','LatestRomCode','CrossRomCode') if k in obj}
    print(json.dumps(result,ensure_ascii=False,indent=2))
if __name__=='__main__': main()
