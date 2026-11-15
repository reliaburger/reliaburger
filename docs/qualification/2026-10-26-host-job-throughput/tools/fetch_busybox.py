import json, urllib.request, tarfile, io, hashlib, sys
repo='docker/library/busybox'; idx='sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028'
tok=json.load(urllib.request.urlopen(f'https://public.ecr.aws/token/?scope=repository:{repo}:pull'))['token']
def get(path, accept):
    r=urllib.request.Request(f'https://public.ecr.aws/v2/{repo}/{path}', headers={'Authorization':'Bearer '+tok,'Accept':accept})
    return urllib.request.urlopen(r).read()
acc='application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json'
ix=json.loads(get('manifests/'+idx, acc))
m=[x for x in ix['manifests'] if x['platform']['architecture']=='arm64' and x['platform'].get('variant','v8')=='v8'][0]
man=json.loads(get('manifests/'+m['digest'], acc))
layer=man['layers'][0]['digest']
blob=get('blobs/'+layer, '*/*')
t=tarfile.open(fileobj=io.BytesIO(blob))
data=t.extractfile('bin/busybox').read()
open(sys.argv[1],'wb').write(data)
print(m['digest'], hashlib.sha256(data).hexdigest())
