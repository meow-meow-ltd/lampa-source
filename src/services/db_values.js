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

export default {
    get
}
