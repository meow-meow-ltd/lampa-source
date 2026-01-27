import Storage from '../core/storage/storage'
import Utils from './utils'

function DeviceID() {
    let id = Storage.get('device_id', '')

    if (!id) {
        id = Utils.uid(16)
        Storage.set('device_id', id)
    }

    return id
}

export default DeviceID
