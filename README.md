



backends:
- gc: https://github.com/fullstorydev/emulators
- s3: https://min.io/
- azure: https://github.com/Azure/Azurite

metadata:
- redis







## extra

maybe use https://github.com/seaweedfs/seaweedfs as s3 


## metadata

```txt                              
keypair ---|                 |--- dir -|- cors
           |                 |         |- acl
keypair ---|--- namespace ---|--- dir
           |                 |         |- cors
keypair ---|                 |--- dir -|- acl
```
