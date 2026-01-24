import Request from '../utils/reguest'

let network = new Request()

function get(key, callback) {
    network.silent('/api/value?key=' + key, (data) => {
        callback(data)
    }, (error) => {
        console.log('ServiceDbValues', 'Error:', error)
        callback(null)
    })
}

function getMany(keys, callback) {
    network.silent('/api/values', (data) => {
        callback(data)
    }, (error) => {
        console.log('ServiceDbValues', 'Error:', error)
        callback({})
    }, {
        keys: keys
    })
}

export default {
    get,
    getMany
}
